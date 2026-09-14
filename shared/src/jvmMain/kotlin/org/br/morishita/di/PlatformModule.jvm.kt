package org.br.morishita.di

import org.br.morishita.WindowsCameraService
import org.br.morishita.domain.CameraService
import org.koin.dsl.module

actual val platformModule = module {
    single<CameraService> { WindowsCameraService() }
}
