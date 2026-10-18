package org.br.morishita.di

import org.br.morishita.JvmNetworkService
import org.br.morishita.NativeCameraService
import org.br.morishita.domain.CameraService
import org.br.morishita.domain.NetworkService
import org.koin.dsl.module

actual val platformModule = module {
    single<CameraService> { NativeCameraService() }
    single<NetworkService> { JvmNetworkService() }
}
