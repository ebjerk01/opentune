package org.opentune.app

import android.app.Application
import android.net.Uri
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.opentune.ffi.Block
import org.opentune.ffi.DefSummary
import org.opentune.ffi.Fix
import org.opentune.ffi.FlashReport
import org.opentune.ffi.Grid
import org.opentune.ffi.Identity
import org.opentune.ffi.ImageInfo
import org.opentune.ffi.MapSummary
import org.opentune.ffi.checksumFix
import org.opentune.ffi.checksumVerify
import org.opentune.ffi.decodeMap
import org.opentune.ffi.flash
import org.opentune.ffi.identify
import org.opentune.ffi.listEcudefs
import org.opentune.ffi.listMaps
import org.opentune.ffi.openImage
import java.io.File

/**
 * Holds UI state and runs every UniFFI call off the main thread.
 *
 * The Rust core reads plain file paths, so a SAF `content://` pick is first
 * copied into the app cache and the local path is handed to the core. The
 * bundled ECU definitions are copied out of assets into [ecudefsDir] once at
 * startup so [org.opentune.ffi.listEcudefs] / [decodeMap] can read them.
 */
class AppViewModel(app: Application) : AndroidViewModel(app) {

    /** On-disk directory holding the ECU-definition JSON files. */
    private val ecudefsDir: File = File(getApplication<Application>().filesDir, "ecudefs")

    // --- Open image ---------------------------------------------------------
    var imagePath by mutableStateOf<String?>(null)
        private set
    var imageInfo by mutableStateOf<ImageInfo?>(null)
        private set

    // --- Identify -----------------------------------------------------------
    var identity by mutableStateOf<Identity?>(null)
        private set

    // --- Checksum -----------------------------------------------------------
    var checksumAlgo by mutableStateOf("edc16")
    var checksumBlocks by mutableStateOf<List<Block>>(emptyList())
        private set
    var checksumFixes by mutableStateOf<List<Fix>?>(null)
        private set

    // --- Maps ---------------------------------------------------------------
    var defs by mutableStateOf<List<DefSummary>>(emptyList())
        private set
    var selectedDef by mutableStateOf<DefSummary?>(null)
        private set
    var maps by mutableStateOf<List<MapSummary>>(emptyList())
        private set
    var grid by mutableStateOf<Grid?>(null)
        private set

    // --- Flash --------------------------------------------------------------
    var flashReport by mutableStateOf<FlashReport?>(null)
        private set

    // --- Shared UI state ----------------------------------------------------
    var busy by mutableStateOf(false)
        private set
    var error by mutableStateOf<String?>(null)
        private set

    init {
        copyAssetDefs()
    }

    fun clearError() {
        error = null
    }

    /** Copy every bundled `assets/ecudefs/*.json` into [ecudefsDir] once. */
    private fun copyAssetDefs() = viewModelScope.launch(Dispatchers.IO) {
        try {
            val am = getApplication<Application>().assets
            ecudefsDir.mkdirs()
            am.list("ecudefs")?.forEach { name ->
                val out = File(ecudefsDir, name)
                am.open("ecudefs/$name").use { input ->
                    out.outputStream().use { input.copyTo(it) }
                }
            }
            reloadDefs()
        } catch (e: Exception) {
            setError("Preparing ECU definitions failed: ${e.message}")
        }
    }

    /**
     * Import a SAF-picked image: copy its bytes into the cache and open it.
     * [writable] copies to a private writable file so Checksum Fix / Flash can
     * persist changes (the picked document itself stays untouched).
     */
    fun openPickedImage(uri: Uri) = run("Opening image") {
        val ctx = getApplication<Application>()
        val name = queryName(uri) ?: "image.bin"
        val dest = File(ctx.cacheDir, "opened_$name")
        ctx.contentResolver.openInputStream(uri)?.use { input ->
            dest.outputStream().use { input.copyTo(it) }
        } ?: throw IllegalStateException("could not read $uri")
        val info = openImage(dest.absolutePath)
        imagePath = dest.absolutePath
        imageInfo = info
        // Reset downstream results when a new image is opened.
        identity = null
        checksumBlocks = emptyList()
        checksumFixes = null
        grid = null
        flashReport = null
    }

    fun runIdentify() = withImage { path ->
        identity = identify(path, ecudefsDir.absolutePath)
    }

    fun runChecksumVerify() = withImage { path ->
        checksumFixes = null
        checksumBlocks = checksumVerify(path, checksumAlgo)
    }

    fun runChecksumFix() = withImage { path ->
        // Writes back in place over the cached copy, then re-verifies.
        checksumFixes = checksumFix(path, checksumAlgo, null)
        checksumBlocks = checksumVerify(path, checksumAlgo)
    }

    fun reloadDefs() = run("Loading definitions") {
        defs = listEcudefs(ecudefsDir.absolutePath)
    }

    fun selectDef(def: DefSummary) = run("Loading maps") {
        selectedDef = def
        maps = listMaps(def.path)
        grid = null
    }

    fun runDecodeMap(mapName: String) = withImage { path ->
        val def = selectedDef ?: throw IllegalStateException("pick an ECU definition first")
        grid = decodeMap(path, def.path, mapName)
    }

    fun runFlash(addr: String, data: ByteArray, minMv: UInt) = withImage { path ->
        flashReport = flash(path, addr, data, minMv)
    }

    // --- helpers ------------------------------------------------------------

    private fun withImage(block: suspend (String) -> Unit) {
        val path = imagePath
        if (path == null) {
            setError("Open a flash image first.")
            return
        }
        run("Working") { block(path) }
    }

    private fun run(what: String, block: suspend () -> Unit) = viewModelScope.launch {
        busy = true
        error = null
        try {
            withContext(Dispatchers.IO) { block() }
        } catch (e: Exception) {
            setError("$what failed: ${e.message}")
        } finally {
            busy = false
        }
    }

    private suspend fun setError(msg: String) = withContext(Dispatchers.Main) {
        error = msg
    }

    private fun queryName(uri: Uri): String? {
        val resolver = getApplication<Application>().contentResolver
        resolver.query(uri, null, null, null, null)?.use { c ->
            val idx = c.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
            if (idx >= 0 && c.moveToFirst()) return c.getString(idx)
        }
        return uri.lastPathSegment?.substringAfterLast('/')
    }
}
