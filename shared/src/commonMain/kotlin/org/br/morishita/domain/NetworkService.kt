package org.br.morishita.domain

data class NetworkAddress(
    val address: String,
    val interfaceName: String,
)

interface NetworkService {
    // IPv4 addresses of the interfaces that are up, loopback included
    fun ipv4Addresses(): List<NetworkAddress>
}
