package org.br.morishita

import org.br.morishita.domain.NetworkAddress
import org.br.morishita.domain.NetworkService
import java.net.Inet4Address
import java.net.NetworkInterface

class JvmNetworkService : NetworkService {
    override fun ipv4Addresses(): List<NetworkAddress> =
        NetworkInterface.networkInterfaces().toList()
            .filter { runCatching { it.isUp }.getOrDefault(false) }
            .flatMap { networkInterface ->
                networkInterface.inetAddresses().toList()
                    .filterIsInstance<Inet4Address>()
                    .map { NetworkAddress(it.hostAddress, networkInterface.displayName) }
            }
            // Real interfaces first, loopback at the end
            .sortedBy { it.address.startsWith("127.") }
}
