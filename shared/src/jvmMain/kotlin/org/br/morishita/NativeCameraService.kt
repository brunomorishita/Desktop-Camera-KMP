package org.br.morishita

import org.br.morishita.domain.CameraService
import org.br.morishita.domain.StreamSettings
import org.br.morishita.domain.StreamTransport
import uniffi.camera_engine.StreamConfig

// Same Rust API on every platform: Windows (Media Foundation) and Linux (V4L2)
class NativeCameraService : CameraService {
    init {
        NativeLibLoader.ensureLoaded()
    }

    private val controller = uniffi.camera_engine.CameraController()

    override val previewSupported: Boolean = controller.previewSupported()

    override fun attachPreview(hwndPointer: ULong, width: UInt, height: UInt) {
        try {
            controller.attachPreview(hwndPointer, width, height)
        } catch (e: Exception) {
            println("Error attaching preview natively: ${e.message}")
            throw e
        }
    }

    override fun detachPreview(hwndPointer: ULong) {
        controller.detachPreview(hwndPointer)
    }

    override fun resizePreview(hwndPointer: ULong, width: UInt, height: UInt) {
        controller.resize(hwndPointer, width, height)
    }

    override suspend fun startCapture() {
        try {
            controller.startCapture()
        } catch (e: Exception) {
            println("Error starting capture natively: ${e.message}")
            throw e
        }
    }

    override suspend fun releaseIfIdle() {
        try {
            controller.releaseIfIdle()
        } catch (e: Exception) {
            println("Error stopping capture natively: ${e.message}")
            throw e
        }
    }

    override suspend fun startStreaming(settings: StreamSettings): String {
        val config = StreamConfig(
            bindAddress = settings.bindAddress,
            port = settings.port,
            path = settings.path,
            transport = when (settings.transport) {
                StreamTransport.Any -> uniffi.camera_engine.StreamTransport.ANY
                StreamTransport.TcpOnly -> uniffi.camera_engine.StreamTransport.TCP_ONLY
                StreamTransport.UdpOnly -> uniffi.camera_engine.StreamTransport.UDP_ONLY
            },
            bitrate = settings.bitrate,
        )
        try {
            return controller.startStreaming(config)
        } catch (e: Exception) {
            println("Error starting RTSP streaming natively: ${e.message}")
            throw e
        }
    }

    override fun stopStreaming() {
        controller.stopStreaming()
    }

    override fun streamingClients(): UInt = controller.streamingClients()
}
