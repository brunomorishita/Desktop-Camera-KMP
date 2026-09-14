package org.br.morishita.domain

interface CameraService {
    fun createRenderer(hwndPointer: ULong, width: UInt, height: UInt)
    suspend fun startCapture()
    suspend fun stopCapture()
    suspend fun renderAllPendingFrames()
}