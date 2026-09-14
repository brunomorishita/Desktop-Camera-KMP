package org.br.morishita

interface Platform {
    val name: String
}

expect fun getPlatform(): Platform