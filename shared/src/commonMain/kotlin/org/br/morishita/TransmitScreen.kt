package org.br.morishita

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import desktop_camera_kmp.shared.generated.resources.Res
import desktop_camera_kmp.shared.generated.resources.settings_24px
import org.jetbrains.compose.resources.vectorResource
import org.koin.compose.viewmodel.koinViewModel

@Composable
fun TransmitScreen(
    cameraViewModel: CameraViewModel = koinViewModel(),
    transmitViewModel: TransmitViewModel = koinViewModel(),
) {
    val state by transmitViewModel.state.collectAsStateWithLifecycle()
    var showSettings by remember { mutableStateOf(false) }

    // Streaming belongs to this screen, like the preview does
    DisposableEffect(Unit) {
        onDispose { transmitViewModel.stop() }
    }

    Column(modifier = Modifier.fillMaxSize()) {
        CameraPreview(
            viewModel = cameraViewModel,
            modifier = Modifier.weight(1f).fillMaxWidth()
        )
        TransmitControls(
            state = state,
            onStart = transmitViewModel::start,
            onStop = transmitViewModel::stop,
            onOpenSettings = { showSettings = true }
        )
    }

    if (showSettings) {
        StreamSettingsDialog(
            settings = state.settings,
            // Read once per opening, so plugging a network cable shows up next time
            addresses = remember { transmitViewModel.networkAddresses() },
            onDismiss = { showSettings = false },
            onSave = {
                transmitViewModel.updateSettings(it)
                showSettings = false
            }
        )
    }
}

@Composable
private fun TransmitControls(
    state: TransmitUiState,
    onStart: () -> Unit,
    onStop: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    Surface(tonalElevation = 2.dp) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(16.dp)
        ) {
            Column(modifier = Modifier.weight(1f)) {
                when (state.status) {
                    TransmitUiState.Status.Stopped -> {
                        Text("Transmissão parada", style = MaterialTheme.typography.titleSmall)
                        state.error?.let {
                            Text(
                                it,
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.error
                            )
                        }
                    }

                    TransmitUiState.Status.Starting -> {
                        Text("Iniciando transmissão…", style = MaterialTheme.typography.titleSmall)
                    }

                    TransmitUiState.Status.Streaming -> {
                        // Selectable so the URL can be copied into VLC/ffplay
                        SelectionContainer {
                            Text(state.url.orEmpty(), style = MaterialTheme.typography.titleSmall)
                        }
                        Text(
                            "${state.clients} cliente(s) conectado(s)",
                            style = MaterialTheme.typography.bodySmall
                        )
                    }
                }
            }

            // Settings apply on the next start, so they are locked while streaming
            IconButton(
                onClick = onOpenSettings,
                enabled = state.status == TransmitUiState.Status.Stopped
            ) {
                Icon(
                    imageVector = vectorResource(Res.drawable.settings_24px),
                    contentDescription = "Configurar transmissão"
                )
            }

            when (state.status) {
                TransmitUiState.Status.Stopped -> Button(onClick = onStart) { Text("Transmitir") }
                TransmitUiState.Status.Starting -> CircularProgressIndicator(
                    modifier = Modifier.size(24.dp),
                    strokeWidth = 2.dp
                )
                TransmitUiState.Status.Streaming -> OutlinedButton(onClick = onStop) { Text("Parar") }
            }
        }
    }
}
