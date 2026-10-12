package org.br.morishita

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.br.morishita.domain.CameraService
import kotlin.time.Duration.Companion.seconds

data class TransmitUiState(
    val status: Status = Status.Stopped,
    val url: String? = null,
    val clients: UInt = 0u,
    val error: String? = null,
) {
    enum class Status { Stopped, Starting, Streaming }
}

class TransmitViewModel(
    private val cameraService: CameraService
) : ViewModel() {

    private val _state = MutableStateFlow(TransmitUiState())
    val state: StateFlow<TransmitUiState> = _state.asStateFlow()

    private var streamingJob: Job? = null

    fun start() {
        if (_state.value.status != TransmitUiState.Status.Stopped) return
        _state.value = TransmitUiState(status = TransmitUiState.Status.Starting)

        streamingJob = viewModelScope.launch(Dispatchers.Default) {
            val url = try {
                cameraService.startStreaming(RTSP_PORT, BITRATE)
            } catch (e: Exception) {
                _state.value = TransmitUiState(error = e.message ?: "Falha ao iniciar a transmissão")
                return@launch
            }
            _state.value = TransmitUiState(status = TransmitUiState.Status.Streaming, url = url)

            // Rust only exposes a counter, polling it is simpler than a callback per connection
            while (isActive) {
                _state.update { it.copy(clients = cameraService.streamingClients()) }
                delay(1.seconds)
            }
        }
    }

    fun stop() {
        val job = streamingJob ?: return
        streamingJob = null
        _state.value = TransmitUiState()

        viewModelScope.launch(Dispatchers.Default) {
            // A start still running must finish (or be cancelled) before stopping
            job.cancelAndJoin()
            cameraService.stopStreaming()
            runCatching { cameraService.releaseIfIdle() }
        }
    }

    companion object {
        const val RTSP_PORT: UShort = 8554u
        const val BITRATE: UInt = 4_000_000u
    }
}
