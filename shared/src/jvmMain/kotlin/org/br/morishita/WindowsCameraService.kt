package org.br.morishita

import org.br.morishita.domain.CameraService

class WindowsCameraService : CameraService {
    init {
        NativeLibLoader.ensureLoaded()
    }

    private val controller = uniffi.camera_engine.CameraController()

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
}
