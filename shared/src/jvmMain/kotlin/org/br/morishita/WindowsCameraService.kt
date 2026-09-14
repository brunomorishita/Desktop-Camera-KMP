package org.br.morishita

import org.br.morishita.domain.CameraService

class WindowsCameraService : CameraService {
    init {
        NativeLibLoader.ensureLoaded()
    }

    private val controller = uniffi.rust_lib.CameraController()

    override fun createRenderer(hwndAddress: ULong, width: UInt, height: UInt) {
        controller.createRenderer(hwndAddress, width, height)
    }

    override suspend fun startCapture() {
        try {
            controller.startCapture()
        } catch (e: Exception) {
            println("Error starting capture natively: ${e.message}")
            throw e
        }
    }

    override suspend fun stopCapture() {
        try {
            controller.stopCapture()
        } catch (e: Exception) {
            println("Error stopping capture natively: ${e.message}")
            throw e
        }
    }

    override suspend fun renderAllPendingFrames() {
        try {
            controller.renderAllPendingFrames()
        } catch (e: Exception) {
            println("Error processing frames natively: ${e.message}")
            throw e
        }
    }
}