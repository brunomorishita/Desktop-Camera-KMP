package org.br.morishita

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.br.morishita.domain.CameraService
import kotlin.time.Duration.Companion.milliseconds

class CameraViewModel(
    private val cameraService: CameraService
) : ViewModel() {

    private val _isStreaming = MutableStateFlow(false)
    val isStreaming: StateFlow<Boolean> = _isStreaming.asStateFlow()

    fun startCamera(hwndPointer: ULong, width: UInt, height: UInt) {
        if (_isStreaming.value) return

        viewModelScope.launch(Dispatchers.Default) {
            cameraService.createRenderer(hwndPointer, width, height)
            cameraService.startCapture()
            _isStreaming.value = true

            cameraService.renderAllPendingFrames()
        }
    }

    fun stopCamera() {
        _isStreaming.value = false
        viewModelScope.launch(Dispatchers.Default) {
            cameraService.stopCapture()
        }
    }

    override fun onCleared() {
        super.onCleared()
        viewModelScope.launch(Dispatchers.Default) {
            cameraService.stopCapture()
        }
//        cameraService.detachHwnd()
    }
}