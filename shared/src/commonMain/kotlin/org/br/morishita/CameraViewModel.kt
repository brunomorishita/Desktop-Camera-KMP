package org.br.morishita

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import org.br.morishita.domain.CameraService

// Camera preview, shared by every screen that shows the camera (each one with its own window)
class CameraViewModel(
    private val cameraService: CameraService
) : ViewModel() {

    val previewSupported: Boolean = cameraService.previewSupported

    // Called on the AWT thread while the window exists
    fun attach(hwndPointer: ULong, width: UInt, height: UInt) {
        try {
            cameraService.attachPreview(hwndPointer, width, height)
        } catch (e: Exception) {
            // Already logged by the service, the window just stays empty
            return
        }

        viewModelScope.launch(Dispatchers.Default) {
            runCatching { cameraService.startCapture() }
        }
    }

    // Called on the AWT thread right before the window is destroyed
    fun detach(hwndPointer: ULong) {
        cameraService.detachPreview(hwndPointer)

        viewModelScope.launch(Dispatchers.Default) {
            runCatching { cameraService.releaseIfIdle() }
        }
    }

    fun resize(hwndPointer: ULong, width: UInt, height: UInt) {
        // Only stores the new size in Rust, cheap enough to call on every layout change
        cameraService.resizePreview(hwndPointer, width, height)
    }
}
