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
import org.br.morishita.domain.NetworkAddress
import org.br.morishita.domain.NetworkService
import org.br.morishita.domain.StreamSettings
import kotlin.time.Duration.Companion.seconds

data class TransmitUiState(
    val status: Status = Status.Stopped,
    val settings: StreamSettings = StreamSettings(),
    val url: String? = null,
    val clients: UInt = 0u,
    val error: String? = null,
) {
    enum class Status { Stopped, Starting, Streaming }
}

class TransmitViewModel(
    private val cameraService: CameraService,
    private val networkService: NetworkService,
) : ViewModel() {

    private val _state = MutableStateFlow(TransmitUiState())
    val state: StateFlow<TransmitUiState> = _state.asStateFlow()

    private var streamingJob: Job? = null

    fun start() {
        if (_state.value.status != TransmitUiState.Status.Stopped) return
        val settings = _state.value.settings
        _state.value = TransmitUiState(status = TransmitUiState.Status.Starting, settings = settings)

        streamingJob = viewModelScope.launch(Dispatchers.Default) {
            val url = try {
                cameraService.startStreaming(settings)
            } catch (e: Exception) {
                _state.value = TransmitUiState(
                    settings = settings,
                    error = e.message ?: "Falha ao iniciar a transmissão"
                )
                return@launch
            }
            _state.value = TransmitUiState(
                status = TransmitUiState.Status.Streaming,
                settings = settings,
                url = url
            )

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
        _state.value = TransmitUiState(settings = _state.value.settings)

        viewModelScope.launch(Dispatchers.Default) {
            // A start still running must finish (or be cancelled) before stopping
            job.cancelAndJoin()
            cameraService.stopStreaming()
            runCatching { cameraService.releaseIfIdle() }
        }
    }

    // Applied on the next start; the running stream keeps its settings
    fun updateSettings(settings: StreamSettings) {
        if (_state.value.status != TransmitUiState.Status.Stopped) return
        _state.update { it.copy(settings = settings, error = null) }
    }

    fun networkAddresses(): List<NetworkAddress> =
        runCatching { networkService.ipv4Addresses() }.getOrDefault(emptyList())
}
