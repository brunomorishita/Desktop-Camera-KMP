package org.br.morishita

import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.*
import androidx.compose.ui.tooling.preview.Preview
import org.br.morishita.di.initKoin

@Composable
@Preview
fun App() {
    initKoin()
    MaterialTheme {
        CameraScreen()
    }
}