package org.opentune.app

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Bolt
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.FolderOpen
import androidx.compose.material.icons.filled.GridOn
import androidx.compose.material.icons.filled.Info
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.lifecycle.viewmodel.compose.viewModel

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            OpenTuneTheme {
                OpenTuneApp()
            }
        }
    }
}

private enum class Screen(val label: String, val icon: ImageVector) {
    Open("Open", Icons.Filled.FolderOpen),
    Identify("Identify", Icons.Filled.Info),
    Checksum("Checksum", Icons.Filled.CheckCircle),
    Maps("Maps", Icons.Filled.GridOn),
    Flash("Flash", Icons.Filled.Bolt),
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun OpenTuneApp(vm: AppViewModel = viewModel()) {
    var current by remember { mutableStateOf(Screen.Open) }
    val snackbar = remember { SnackbarHostState() }

    // Surface FFI errors as a snackbar.
    LaunchedEffect(vm.error) {
        vm.error?.let {
            snackbar.showSnackbar(it)
            vm.clearError()
        }
    }

    Scaffold(
        topBar = { TopAppBar(title = { Text("OpenTune — ${current.label}") }) },
        snackbarHost = { SnackbarHost(snackbar) },
        bottomBar = {
            NavigationBar {
                Screen.entries.forEach { s ->
                    NavigationBarItem(
                        selected = current == s,
                        onClick = { current = s },
                        icon = { Icon(s.icon, contentDescription = s.label) },
                        label = { Text(s.label) },
                    )
                }
            }
        },
    ) { padding ->
        Box(Modifier.padding(padding)) {
            if (vm.busy) {
                LinearProgressIndicator(
                    Modifier.fillMaxWidth().align(Alignment.TopCenter),
                )
            }
            when (current) {
                Screen.Open -> OpenScreen(vm)
                Screen.Identify -> IdentifyScreen(vm)
                Screen.Checksum -> ChecksumScreen(vm)
                Screen.Maps -> MapsScreen(vm)
                Screen.Flash -> FlashScreen(vm)
            }
        }
    }
}
