package org.opentune.app

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import org.opentune.ffi.Grid

// ---------------------------------------------------------------------------
// Shared building blocks
// ---------------------------------------------------------------------------

@Composable
private fun ScreenColumn(content: @Composable () -> Unit) {
    Column(
        Modifier
            .fillMaxWidth()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) { content() }
}

@Composable
private fun SectionCard(title: String, content: @Composable () -> Unit) {
    Card(elevation = CardDefaults.cardElevation(defaultElevation = 2.dp)) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(title, style = MaterialTheme.typography.titleMedium)
            content()
        }
    }
}

@Composable
private fun KeyValue(key: String, value: String?) {
    Row(Modifier.fillMaxWidth()) {
        Text(key, Modifier.width(120.dp), style = MaterialTheme.typography.labelLarge)
        Text(
            value ?: "—",
            style = MaterialTheme.typography.bodyMedium,
            fontFamily = if (key.endsWith(":")) FontFamily.Monospace else FontFamily.Default,
        )
    }
}

@Composable
private fun Mono(text: String) {
    Text(text, fontFamily = FontFamily.Monospace, fontSize = 13.sp)
}

// ---------------------------------------------------------------------------
// 1. Open image (SAF file picker)
// ---------------------------------------------------------------------------

@Composable
fun OpenScreen(vm: AppViewModel) {
    val picker = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocument(),
    ) { uri -> uri?.let { vm.openPickedImage(it) } }

    ScreenColumn {
        SectionCard("Flash image") {
            Text(
                "Open a raw ECU flash image (.bin). It is copied into the app's " +
                    "private storage so the offline workflow can read and rewrite it; " +
                    "the picked file itself is never modified.",
                style = MaterialTheme.typography.bodyMedium,
            )
            Button(onClick = { picker.launch(arrayOf("*/*")) }) {
                Text("Open image…")
            }
        }
        vm.imageInfo?.let { info ->
            SectionCard("Opened") {
                KeyValue("Name", info.name)
                KeyValue("Size", "${info.size} bytes")
                Mono(info.path)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Identify
// ---------------------------------------------------------------------------

@Composable
fun IdentifyScreen(vm: AppViewModel) {
    ScreenColumn {
        SectionCard("Identify") {
            Text(
                "Reads the ECU identity from the opened image and matches it " +
                    "against the bundled ECU definitions.",
                style = MaterialTheme.typography.bodyMedium,
            )
            Button(onClick = { vm.runIdentify() }, enabled = vm.imagePath != null) {
                Text("Identify")
            }
        }
        vm.identity?.let { id ->
            SectionCard("Reported identity") {
                KeyValue("ECU name", id.ecuName)
                KeyValue("HW / part", id.hwNumber)
                KeyValue("SW number", id.swNumber)
                KeyValue("Supplier", id.supplier)
                KeyValue("VIN", id.vin)
                KeyValue("Size", "${id.size} bytes")
            }
            id.matched?.let { m ->
                SectionCard("Matched definition") {
                    KeyValue("Id", m.id)
                    KeyValue("Name", m.name)
                    KeyValue("Maker", m.maker)
                    KeyValue("Family", m.family)
                    KeyValue("Protocol", m.protocol)
                    KeyValue("Checksum", m.checksum)
                    KeyValue("Seed/key", m.seedkey)
                }
            } ?: Text(
                "No definition matched (the offline driver reports only a name + size).",
                style = MaterialTheme.typography.bodySmall,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Checksum
// ---------------------------------------------------------------------------

private val CHECKSUM_ALGOS = listOf("edc16", "edc17", "pcr21", "crc32-tail", "edc15c6")

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChecksumScreen(vm: AppViewModel) {
    var expanded by remember { mutableStateOf(false) }

    ScreenColumn {
        SectionCard("Algorithm") {
            ExposedDropdownMenuBox(
                expanded = expanded,
                onExpandedChange = { expanded = it },
            ) {
                OutlinedTextField(
                    value = vm.checksumAlgo,
                    onValueChange = {},
                    readOnly = true,
                    label = { Text("Checksum algorithm") },
                    trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded) },
                    modifier = Modifier.menuAnchor().fillMaxWidth(),
                )
                ExposedDropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
                    CHECKSUM_ALGOS.forEach { algo ->
                        DropdownMenuItem(
                            text = { Text(algo) },
                            onClick = {
                                vm.checksumAlgo = algo
                                expanded = false
                            },
                        )
                    }
                }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = { vm.runChecksumVerify() }, enabled = vm.imagePath != null) {
                    Text("Verify")
                }
                OutlinedButton(onClick = { vm.runChecksumFix() }, enabled = vm.imagePath != null) {
                    Text("Fix")
                }
            }
        }

        if (vm.checksumBlocks.isNotEmpty()) {
            SectionCard("Blocks") {
                vm.checksumBlocks.forEach { b ->
                    Column {
                        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                            Text(
                                if (b.ok) "OK " else "BAD",
                                color = if (b.ok) Color(0xFF2E7D32) else MaterialTheme.colorScheme.error,
                                fontWeight = FontWeight.Bold,
                                fontFamily = FontFamily.Monospace,
                            )
                            Text("  ${b.name}", style = MaterialTheme.typography.bodyMedium)
                        }
                        Mono("  ${b.region}")
                        Mono("  stored ${b.stored}  computed ${b.computed}")
                    }
                }
            }
        }

        vm.checksumFixes?.let { fixes ->
            SectionCard("Fixes") {
                if (fixes.isEmpty()) {
                    Text("No corrections needed.")
                } else {
                    fixes.forEach { f ->
                        Mono(
                            "${f.name} @0x%08X: 0x%08X -> 0x%08X"
                                .format(f.offset.toLong(), f.old.toLong(), f.new.toLong()),
                        )
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Maps — read-only heat grid
// ---------------------------------------------------------------------------

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MapsScreen(vm: AppViewModel) {
    var defExpanded by remember { mutableStateOf(false) }

    ScreenColumn {
        SectionCard("ECU definition") {
            ExposedDropdownMenuBox(
                expanded = defExpanded,
                onExpandedChange = { defExpanded = it },
            ) {
                OutlinedTextField(
                    value = vm.selectedDef?.name ?: "Select…",
                    onValueChange = {},
                    readOnly = true,
                    label = { Text("Definition") },
                    trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(defExpanded) },
                    modifier = Modifier.menuAnchor().fillMaxWidth(),
                )
                ExposedDropdownMenu(
                    expanded = defExpanded,
                    onDismissRequest = { defExpanded = false },
                ) {
                    vm.defs.forEach { def ->
                        DropdownMenuItem(
                            text = { Text("${def.name}  (${def.mapCount} maps)") },
                            onClick = {
                                vm.selectDef(def)
                                defExpanded = false
                            },
                        )
                    }
                }
            }
            if (vm.defs.isEmpty()) {
                OutlinedButton(onClick = { vm.reloadDefs() }) { Text("Reload definitions") }
            }
        }

        if (vm.maps.isNotEmpty()) {
            SectionCard("Maps") {
                Text(
                    "Tap a map to decode it from the opened image.",
                    style = MaterialTheme.typography.bodySmall,
                )
                vm.maps.forEach { m ->
                    OutlinedButton(
                        onClick = { vm.runDecodeMap(m.name) },
                        enabled = vm.imagePath != null,
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        Text("${m.name}  ·  ${m.rows}×${m.cols} ${m.dataType}", textAlign = TextAlign.Start)
                    }
                }
            }
        }

        vm.grid?.let { grid -> HeatGridCard(grid) }
    }
}

@Composable
private fun HeatGridCard(grid: Grid) {
    SectionCard(grid.name) {
        KeyValue("Address", grid.address)
        KeyValue("Size", "${grid.rows}×${grid.cols} ${grid.dataType}/${grid.endian}")
        KeyValue("Unit", grid.unit.ifEmpty { "—" })
        val lo = grid.min ?: 0.0
        val hi = grid.max ?: 0.0
        KeyValue("Range", "%.3f … %.3f".format(lo, hi))
        if (grid.notes.isNotEmpty()) {
            Text(grid.notes, style = MaterialTheme.typography.bodySmall)
        }
        HeatGrid(grid, lo, hi)
    }
}

/** Blue (cool/low) → amber → red (hot/high) ramp for a normalised value. */
private fun heatColor(t: Float): Color {
    val cool = Color(0xFF1565C0)
    val mid = Color(0xFFFFC107)
    val hot = Color(0xFFC62828)
    return if (t < 0.5f) lerp(cool, mid, t * 2f) else lerp(mid, hot, (t - 0.5f) * 2f)
}

@Composable
private fun HeatGrid(grid: Grid, lo: Double, hi: Double) {
    val span = (hi - lo).takeIf { it > 1e-9 } ?: 1.0
    Column(
        Modifier.horizontalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        grid.cells.forEach { row ->
            Row(horizontalArrangement = Arrangement.spacedBy(2.dp)) {
                row.forEach { v ->
                    val t = (((v - lo) / span).toFloat()).coerceIn(0f, 1f)
                    val bg = heatColor(t)
                    Surface(
                        color = bg,
                        shape = RoundedCornerShape(3.dp),
                        modifier = Modifier.size(width = 44.dp, height = 30.dp),
                    ) {
                        Box(contentAlignment = Alignment.Center) {
                            Text(
                                "%.1f".format(v),
                                fontSize = 9.sp,
                                color = if (t > 0.55f) Color.White else Color.Black,
                                fontFamily = FontFamily.Monospace,
                            )
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Flash (mock, safety-gated)
// ---------------------------------------------------------------------------

@Composable
fun FlashScreen(vm: AppViewModel) {
    var addr by remember { mutableStateOf("0x0") }
    var hexBytes by remember { mutableStateOf("11 22 33 44") }
    var minMv by remember { mutableStateOf("12000") }

    ScreenColumn {
        SectionCard("Mock flash (offline)") {
            Text(
                "Writes bytes into the opened image through the full safety " +
                    "orchestrator: voltage gate → verified backup → write → read-back " +
                    "verify. No hardware is touched — the mock driver treats the file " +
                    "as the ECU.",
                style = MaterialTheme.typography.bodyMedium,
            )
            OutlinedTextField(
                value = addr,
                onValueChange = { addr = it },
                label = { Text("Address (hex, e.g. 0x3C000)") },
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = hexBytes,
                onValueChange = { hexBytes = it },
                label = { Text("Bytes to write (hex, space/comma separated)") },
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = minMv,
                onValueChange = { minMv = it },
                label = { Text("Min battery mV (gate)") },
                modifier = Modifier.fillMaxWidth(),
            )
            Button(
                onClick = {
                    val data = parseHexBytes(hexBytes)
                    val mv = minMv.trim().toUIntOrNull() ?: 12000u
                    if (data.isEmpty()) {
                        // Nothing to write; let the VM surface a friendly error.
                        vm.runFlash(addr, ByteArray(0), mv)
                    } else {
                        vm.runFlash(addr, data, mv)
                    }
                },
                enabled = vm.imagePath != null,
            ) { Text("Flash") }
        }

        vm.flashReport?.let { r ->
            SectionCard("Flash report") {
                KeyValue("Region", r.region)
                KeyValue("Bytes written", r.bytesWritten.toString())
                KeyValue("Backup", "${r.backupLen} bytes")
                KeyValue("Battery", r.batteryMv?.let { "%.2f V".format(it.toInt() / 1000.0) })
                KeyValue("Verified", r.verified.toString())
            }
        }
    }
}

/** Parse "11 22,33  0xAA" into bytes; ignores blanks and 0x prefixes. */
private fun parseHexBytes(s: String): ByteArray =
    s.split(' ', ',', '\n', '\t')
        .map { it.trim().removePrefix("0x").removePrefix("0X") }
        .filter { it.isNotEmpty() }
        .mapNotNull { it.toIntOrNull(16)?.toByte() }
        .toByteArray()
