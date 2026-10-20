package io.github.cyberhirsch.rogueim.phone

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test

class LinkTest {
    /** The same fixed values as `known_answer` in plugins/phone/src/link.rs. */
    @Test
    fun matchesRogueIM() {
        val key = Half.derive(ByteArray(32) { 0x11 }, "c2s", ByteArray(16) { 0x22 }, ByteArray(16) { 0x33 })
        val frame = Half(key).seal("{\"t\":\"ping\"}".toByteArray())
        assertEquals("0000001c1852942c95f4b748a154a63284cdad89ff2843616a1d117d0e4fa3f6", frame.joinToString("") { "%02x".format(it) })
    }

    @Test
    fun opensWhatItSeals() {
        val k = ByteArray(32) { it.toByte() }
        val a = Half(k)
        val b = Half(k)
        repeat(3) { i ->
            val f = a.seal("message $i".toByteArray())
            assertArrayEquals("message $i".toByteArray(), b.open(f.copyOfRange(4, f.size)))
        }
    }
}
