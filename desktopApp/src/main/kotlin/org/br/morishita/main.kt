package org.br.morishita

import androidx.compose.ui.window.Window
import androidx.compose.ui.window.application
import org.br.morishita.di.initKoin

fun main() = application {
    initKoin()
    Window(
        onCloseRequest = ::exitApplication,
        title = "Desktop-Camera-KMP",
    ) {
        App()
    }
}