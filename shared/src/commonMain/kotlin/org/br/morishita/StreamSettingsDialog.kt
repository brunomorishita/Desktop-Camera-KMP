package org.br.morishita

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuAnchorType
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import org.br.morishita.domain.NetworkAddress
import org.br.morishita.domain.StreamSettings
import org.br.morishita.domain.StreamTransport

@Composable
fun StreamSettingsDialog(
    settings: StreamSettings,
    addresses: List<NetworkAddress>,
    onDismiss: () -> Unit,
    onSave: (StreamSettings) -> Unit,
) {
    var bindAddress by remember { mutableStateOf(settings.bindAddress) }
    var port by remember { mutableStateOf(settings.port.toString()) }
    var path by remember { mutableStateOf(settings.path) }
    var transport by remember { mutableStateOf(settings.transport) }

    val parsedPort = port.toIntOrNull()?.takeIf { it in 1..65535 }
    val cleanPath = path.trim().trim('/')
    val pathValid = cleanPath.isNotEmpty() && cleanPath.none { it.isWhitespace() }

    val options = buildList {
        add(NetworkAddress(StreamSettings.ALL_INTERFACES, "Todas as interfaces"))
        addAll(addresses)
        // Keeps a saved address that is not available right now (e.g. Wi-Fi off)
        if (none { it.address == bindAddress }) add(NetworkAddress(bindAddress, "Indisponível"))
    }
    // Listening everywhere: preview the first real address, like the server does
    val urlHost = if (bindAddress == StreamSettings.ALL_INTERFACES) {
        addresses.firstOrNull { !it.address.startsWith("127.") }?.address ?: "<seu-ip>"
    } else {
        bindAddress
    }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Configurar transmissão") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                AddressDropdown(
                    options = options,
                    selected = bindAddress,
                    onSelect = { bindAddress = it }
                )
                OutlinedTextField(
                    value = port,
                    onValueChange = { value -> port = value.filter { it.isDigit() }.take(5) },
                    label = { Text("Porta") },
                    singleLine = true,
                    isError = parsedPort == null,
                    supportingText = if (parsedPort == null) {
                        { Text("Entre 1 e 65535") }
                    } else null,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    modifier = Modifier.fillMaxWidth()
                )
                OutlinedTextField(
                    value = path,
                    onValueChange = { path = it },
                    label = { Text("Caminho") },
                    singleLine = true,
                    isError = !pathValid,
                    supportingText = if (!pathValid) {
                        { Text("Não pode ser vazio nem ter espaços") }
                    } else null,
                    modifier = Modifier.fillMaxWidth()
                )
                Text("Transporte", style = MaterialTheme.typography.labelLarge)
                TransportSelector(selected = transport, onSelect = { transport = it })
                Text(
                    "rtsp://$urlHost:$port/$cleanPath",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
            }
        },
        confirmButton = {
            TextButton(
                enabled = parsedPort != null && pathValid,
                onClick = {
                    onSave(
                        settings.copy(
                            bindAddress = bindAddress,
                            port = parsedPort!!.toUShort(),
                            path = cleanPath,
                            transport = transport,
                        )
                    )
                }
            ) { Text("Salvar") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancelar") }
        }
    )
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun AddressDropdown(
    options: List<NetworkAddress>,
    selected: String,
    onSelect: (String) -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    val current = options.first { it.address == selected }

    ExposedDropdownMenuBox(expanded = expanded, onExpandedChange = { expanded = it }) {
        OutlinedTextField(
            value = current.label(),
            onValueChange = {},
            readOnly = true,
            singleLine = true,
            label = { Text("Interface") },
            trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded = expanded) },
            modifier = Modifier
                .menuAnchor(ExposedDropdownMenuAnchorType.PrimaryNotEditable)
                .fillMaxWidth()
        )
        ExposedDropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
            options.forEach { option ->
                DropdownMenuItem(
                    text = { Text(option.label()) },
                    onClick = {
                        onSelect(option.address)
                        expanded = false
                    }
                )
            }
        }
    }
}

private fun NetworkAddress.label() = "$address ($interfaceName)"

@Composable
private fun TransportSelector(
    selected: StreamTransport,
    onSelect: (StreamTransport) -> Unit,
) {
    val labels = listOf(
        StreamTransport.Any to "UDP e TCP",
        StreamTransport.TcpOnly to "Só TCP",
        StreamTransport.UdpOnly to "Só UDP",
    )
    SingleChoiceSegmentedButtonRow(modifier = Modifier.fillMaxWidth()) {
        labels.forEachIndexed { index, (transport, label) ->
            SegmentedButton(
                selected = selected == transport,
                onClick = { onSelect(transport) },
                shape = SegmentedButtonDefaults.itemShape(index = index, count = labels.size)
            ) { Text(label) }
        }
    }
}
