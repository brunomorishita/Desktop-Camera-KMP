package org.br.morishita

import androidx.compose.runtime.*
import org.koin.compose.viewmodel.koinViewModel

@Composable
fun CameraScreen(
    viewModel: CameraViewModel = koinViewModel()
) {
    val isStreaming by viewModel.isStreaming.collectAsState()

    CameraView(
        onCanvasReady = { hwndPtr, width, height ->
            viewModel.startCamera(hwndPtr, width, height)
        },
        onSizeChanged = { width, height ->
//            viewModel.resize(width, height)
        }
    )
}