package org.br.morishita

import org.br.morishita.domain.CameraService

class WindowsCameraService : CameraService {
    init {
        NativeLibLoader.ensureLoaded()
    }

    private val controller = uniffi.camera_engine.CameraController()

    override suspend fun start(hwndAddress: ULong, width: UInt, height: UInt) {
        try {
            controller.start(hwndAddress, width, height)
        } catch (e: Exception) {
            println("Error starting capture natively: ${e.message}")
            throw e
        }
    }

    override suspend fun stop() {
        try {
            controller.stop()
        } catch (e: Exception) {
            println("Error stopping capture natively: ${e.message}")
            throw e
        }
    }
}