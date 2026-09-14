package org.br.morishita

import androidx.compose.ui.window.Window
import androidx.compose.ui.window.application

fun main() = application {
    Window(
        onCloseRequest = ::exitApplication,
        title = "Desktop-Camera-KMP",
    ) {
        App()
    }
}