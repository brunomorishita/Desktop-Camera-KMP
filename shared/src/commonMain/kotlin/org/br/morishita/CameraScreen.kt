package org.br.morishita

import androidx.compose.runtime.*
import org.koin.compose.viewmodel.koinViewModel

@Composable
fun CameraScreen(
    viewModel: CameraViewModel = koinViewModel()
) {
    DisposableEffect(Unit) {
        onDispose {
            viewModel.stop()
        }
    }

    CameraView(
        onCanvasReady = { hwndPtr, width, height ->
            viewModel.start(hwndPtr, width, height)
        },
        onSizeChanged = { width, height ->
//            viewModel.resize(width, height)
        }
    )
}