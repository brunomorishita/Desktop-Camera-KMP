package org.br.morishita

import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
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
