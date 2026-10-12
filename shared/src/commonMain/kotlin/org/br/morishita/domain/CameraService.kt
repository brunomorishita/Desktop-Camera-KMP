package org.br.morishita.domain

interface CameraService {
    // Preview: synchronous, must be called while the native window exists
    fun attachPreview(hwndPointer: ULong, width: UInt, height: UInt)
    fun detachPreview(hwndPointer: ULong)
    fun resizePreview(hwndPointer: ULong, width: UInt, height: UInt)

    // Camera starts only if some preview or stream is using it
    suspend fun startCapture()
    suspend fun releaseIfIdle()

    // RTSP: returns the stream URL
    suspend fun startStreaming(port: UShort, bitrate: UInt): String
    fun stopStreaming()
    fun streamingClients(): UInt
}
