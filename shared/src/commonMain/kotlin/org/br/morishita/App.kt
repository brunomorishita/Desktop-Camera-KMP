package org.br.morishita

import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.tooling.preview.Preview
import androidx.navigation3.runtime.NavEntry
import androidx.navigation3.runtime.NavKey
import androidx.navigation3.ui.NavDisplay
import desktop_camera_kmp.shared.generated.resources.Res
import desktop_camera_kmp.shared.generated.resources.camera_video_24px
import desktop_camera_kmp.shared.generated.resources.sensors_24px
import kotlinx.serialization.Serializable
import org.br.morishita.di.initKoin
import org.jetbrains.compose.resources.DrawableResource
import org.jetbrains.compose.resources.vectorResource

@Serializable
sealed interface Route : NavKey {
    @Serializable
    data object View : Route

    @Serializable
    data object Transmit : Route
}

enum class TopLevelDestination(
    val route: Route,
    val icon: DrawableResource,
    val label: String
) {
    VIEW(Route.View, Res.drawable.camera_video_24px, "Ver"),
    TRANSMIT(Route.Transmit, Res.drawable.sensors_24px, "Transmitir"),
}

@Composable
@Preview
fun App() {
    val backStack = remember { mutableStateListOf<Route>(Route.View) }
    val currentRoute = backStack.lastOrNull()

    MaterialTheme {
        Scaffold(
            contentWindowInsets = WindowInsets(0, 0, 0, 0),
            bottomBar = {
                NavigationBar {
                    TopLevelDestination.entries.forEach { destination ->
                        val isSelected = currentRoute == destination.route

                        NavigationBarItem(
                            selected = isSelected,
                            onClick = {
                                if (!isSelected) {
                                    backStack.clear()
                                    backStack.add(destination.route)
                                }
                            },
                            icon = { Icon(imageVector = vectorResource(destination.icon),
                                contentDescription = destination.label
                            ) },
                            label = { Text(destination.label) }
                        )
                    }
                }
            }
        ) { innerPadding ->
            NavDisplay(
                backStack = backStack,
                modifier = Modifier.padding(innerPadding),
                onBack = { backStack.removeLastOrNull() },
                entryProvider = { key ->
                    when (key) {
                        is Route.View -> NavEntry(key) {
                            CameraScreen()
                        }
                        is Route.Transmit -> NavEntry(key) {
                            Text("NOT IMPLEMENTED YET")
                        }
                    }
                }
            )
        }
    }
}