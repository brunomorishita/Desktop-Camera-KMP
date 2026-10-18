package org.br.morishita

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import org.koin.compose.viewmodel.koinViewModel

@Composable
fun CameraScreen(
    viewModel: CameraViewModel = koinViewModel()
) {
    CameraPreview(viewModel)
}

@Composable
fun CameraPreview(
    viewModel: CameraViewModel,
    modifier: Modifier = Modifier
) {
    if (!viewModel.previewSupported) {
        PreviewUnavailable(modifier)
        return
    }

    // Camera lifecycle follows the native window: attached on creation, detached before destruction
    CameraView(
        onCanvasReady = { hwndPtr, width, height ->
            viewModel.attach(hwndPtr, width, height)
        },
        onCanvasDisposed = { hwndPtr ->
            viewModel.detach(hwndPtr)
        },
        onSizeChanged = { hwndPtr, width, height ->
            viewModel.resize(hwndPtr, width, height)
        },
        modifier = modifier
    )
}

// Streaming still works without it: the camera starts with the stream
@Composable
private fun PreviewUnavailable(modifier: Modifier = Modifier) {
    Box(
        modifier = modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.surfaceVariant),
        contentAlignment = Alignment.Center
    ) {
        Text(
            text = "A pré-visualização da câmera não está disponível neste sistema.\n" +
                "No Linux ela precisa de uma sessão X11 (ou XWayland) e de uma GPU que\n" +
                "decodifique H.264 ou JPEG por VA-API. A transmissão RTSP continua disponível.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(24.dp)
        )
    }
}
