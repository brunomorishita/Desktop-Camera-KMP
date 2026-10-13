package org.br.morishita.domain

enum class StreamTransport { Any, TcpOnly, UdpOnly }

data class StreamSettings(
    // IPv4 address to listen on, ALL_INTERFACES for every interface
    val bindAddress: String = ALL_INTERFACES,
    val port: UShort = 8554u,
    val path: String = "camera",
    val transport: StreamTransport = StreamTransport.Any,
    val bitrate: UInt = 4_000_000u,
) {
    companion object {
        const val ALL_INTERFACES = "0.0.0.0"
    }
}
