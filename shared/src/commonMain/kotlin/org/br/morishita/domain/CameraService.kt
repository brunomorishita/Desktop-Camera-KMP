package org.br.morishita.domain

interface CameraService {
    suspend fun start(hwndPointer: ULong, width: UInt, height: UInt)
    suspend fun stop()
}