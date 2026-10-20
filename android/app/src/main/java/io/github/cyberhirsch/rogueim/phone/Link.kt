package io.github.cyberhirsch.rogueim.phone

import android.util.Base64
import org.json.JSONObject
import java.io.DataInputStream
import java.io.OutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.nio.ByteBuffer
import java.security.SecureRandom
import java.util.concurrent.LinkedBlockingQueue
import javax.crypto.Cipher
import javax.crypto.Mac
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/** What the QR code in RogueIM carries: `rimphone:1:<ip>:<port>:<key>:<name>`. */
data class Pairing(val host: String, val port: Int, val key: ByteArray, val name: String) {
    companion object {
        fun parse(code: String): Pairing? {
            val p = code.trim().split(":", limit = 6)
            if (p.size < 6 || p[0] != "rimphone" || p[1] != "1") return null
            val key = try {
                Base64.decode(p[4], Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
            } catch (e: IllegalArgumentException) {
                return null
            }
            val port = p[3].toIntOrNull() ?: return null
            if (key.size != 32) return null
            return Pairing(p[2], port, key, p[5])
        }
    }
}

/**
 * One direction of the link: AES-256-GCM with a counter nonce (see
 * plugins/phone/src/link.rs in RogueIM, which this mirrors byte for byte).
 */
class Half(key: ByteArray) {
    private val key = SecretKeySpec(key, "AES")
    private var counter = 0L

    private fun nonce(): ByteArray = ByteBuffer.allocate(12).putInt(0).putLong(counter++).array()

    fun seal(plain: ByteArray): ByteArray {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key, GCMParameterSpec(128, nonce()))
        val ct = c.doFinal(plain)
        return ByteBuffer.allocate(4 + ct.size).putInt(ct.size).put(ct).array()
    }

    fun open(ct: ByteArray): ByteArray {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, nonce()))
        return c.doFinal(ct)
    }

    companion object {
        fun derive(key: ByteArray, label: String, nc: ByteArray, ns: ByteArray): ByteArray {
            val mac = Mac.getInstance("HmacSHA256")
            mac.init(SecretKeySpec(key, "HmacSHA256"))
            mac.update(label.toByteArray())
            mac.update(nc)
            mac.update(ns)
            return mac.doFinal()
        }
    }
}

/**
 * The connection to RogueIM: keeps trying while paired, sends what is
 * queued, hands what arrives to [onMessage].
 */
class Link(private val device: String, private val onMessage: (JSONObject) -> Unit, private val onState: (Boolean) -> Unit) {
    private val queue = LinkedBlockingQueue<JSONObject>(500)
    @Volatile var pairing: Pairing? = null
    @Volatile var connected = false
        private set
    @Volatile private var out: Pair<OutputStream, Half>? = null
    @Volatile private var running = true
    private val wake = Object()

    fun start() {
        Thread({ loop() }, "rogueim-link").apply { isDaemon = true }.start()
    }

    fun stop() {
        running = false
        poke()
    }

    /** Try now instead of waiting (a new notification, a new pairing). */
    fun poke() = synchronized(wake) { wake.notifyAll() }

    fun send(m: JSONObject) {
        val o = out
        if (o != null && write(o, m)) return
        if (!queue.offer(m)) {
            queue.poll()
            queue.offer(m)
        }
        poke()
    }

    private fun write(o: Pair<OutputStream, Half>, m: JSONObject): Boolean = try {
        synchronized(o.first) {
            o.first.write(o.second.seal(m.toString().toByteArray()))
            o.first.flush()
        }
        true
    } catch (e: Exception) {
        false
    }

    private fun loop() {
        while (running) {
            val p = pairing
            if (p != null) {
                try {
                    session(p)
                } catch (e: Exception) {
                    // Away from home, the computer off: try again later.
                }
                out = null
                if (connected) {
                    connected = false
                    onState(false)
                }
            }
            synchronized(wake) { wake.wait(20_000) }
        }
    }

    private fun session(p: Pairing) {
        Socket().use { s ->
            s.connect(InetSocketAddress(p.host, p.port), 5_000)
            s.soTimeout = 95_000
            val input = DataInputStream(s.getInputStream())
            val output = s.getOutputStream()
            val nc = ByteArray(16).also { SecureRandom().nextBytes(it) }
            output.write(byteArrayOf('R'.code.toByte(), 'I'.code.toByte(), 'M'.code.toByte(), 'P'.code.toByte(), 1) + nc)
            val reply = ByteArray(21)
            input.readFully(reply)
            if (String(reply, 0, 4) != "RIMP" || reply[4].toInt() != 1) throw IllegalStateException("not RogueIM")
            val ns = reply.copyOfRange(5, 21)
            val up = Half(Half.derive(p.key, "c2s", nc, ns))
            val down = Half(Half.derive(p.key, "s2c", nc, ns))
            val o = Pair(output, up)
            if (!write(o, JSONObject().put("t", "hello").put("device", device))) return
            out = o
            connected = true
            onState(true)
            while (true) {
                val m = queue.poll() ?: break
                if (!write(o, m)) return
            }
            while (running && pairing == p) {
                val len = input.readInt()
                if (len < 0 || len > (1 shl 20)) return
                val f = ByteArray(len)
                input.readFully(f)
                val m = JSONObject(String(down.open(f)))
                if (m.optString("t") == "ping") write(o, JSONObject().put("t", "pong")) else onMessage(m)
            }
        }
    }
}
