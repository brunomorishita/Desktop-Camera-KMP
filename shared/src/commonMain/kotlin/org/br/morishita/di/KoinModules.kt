package org.br.morishita.di

import org.br.morishita.CameraViewModel
import org.koin.core.context.GlobalContext.startKoin
import org.koin.core.module.dsl.viewModel
import org.koin.dsl.module
import org.koin.dsl.KoinAppDeclaration
import org.koin.core.module.Module

val viewModelModule = module {
    viewModel { CameraViewModel(cameraService = get()) }
}

expect val platformModule: Module

fun initKoin(appDeclaration: KoinAppDeclaration = {}) = startKoin {
    appDeclaration()
    modules(viewModelModule, platformModule)
}