package io.github.cyberhirsch.rogueim.phone

import android.app.Notification
import android.app.RemoteInput
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import androidx.core.app.NotificationCompat
import org.json.JSONArray
import org.json.JSONObject
import java.security.MessageDigest

/** Messengers the app knows; the user picks which of the installed ones to forward. */
val KNOWN_APPS = linkedMapOf(
    "com.whatsapp" to "WhatsApp",
    "com.whatsapp.w4b" to "WhatsApp Business",
    "org.thoughtcrime.securesms" to "Signal",
    "org.telegram.messenger" to "Telegram",
    "com.google.android.apps.messaging" to "Messages",
    "com.samsung.android.messaging" to "Samsung Messages",
    "ch.threema.app" to "Threema",
    "im.vector.app" to "Element",
    "com.facebook.orca" to "Messenger",
    "com.instagram.android" to "Instagram",
    "com.discord" to "Discord",
    "com.Slack" to "Slack",
    "com.microsoft.teams" to "Teams",
)
val DEFAULT_ON = setOf("com.whatsapp", "com.whatsapp.w4b", "org.thoughtcrime.securesms", "org.telegram.messenger", "com.google.android.apps.messaging", "com.samsung.android.messaging", "ch.threema.app")

object Prefs {
    private fun sp(c: Context) = c.getSharedPreferences("rogueim", Context.MODE_PRIVATE)
    fun pairing(c: Context): Pairing? = sp(c).getString("code", null)?.let { Pairing.parse(it) }
    fun setCode(c: Context, code: String?) = sp(c).edit().putString("code", code).apply()
    fun enabled(c: Context, pkg: String) = sp(c).getBoolean("app:$pkg", pkg in DEFAULT_ON)
    fun setEnabled(c: Context, pkg: String, on: Boolean) = sp(c).edit().putBoolean("app:$pkg", on).apply()
}

/** The app's one link to RogueIM, shared by the listener and the screen. */
object Hub {
    var link: Link? = null
    var onChange: (() -> Unit)? = null
}

/**
 * Reads the notifications of the chosen messengers and forwards their
 * messages; answers come back through the notification's reply button.
 */
class Forwarder : NotificationListenerService() {
    /** The newest notification of each chat, to reply to and to mark read. */
    private val latest = HashMap<String, StatusBarNotification>()
    private val seen = object : LinkedHashMap<String, Boolean>(512, 0.75f, true) {
        override fun removeEldestEntry(e: MutableMap.MutableEntry<String, Boolean>) = size > 2000
    }

    override fun onListenerConnected() {
        val link = Link("${Build.MANUFACTURER} ${Build.MODEL}".trim(), ::fromComputer) { Hub.onChange?.invoke() }
        link.pairing = Prefs.pairing(this)
        link.start()
        Hub.link = link
        sendApps()
        activeNotifications?.forEach { onNotificationPosted(it) }
    }

    override fun onListenerDisconnected() {
        Hub.link?.stop()
        Hub.link = null
    }

    fun sendApps() {
        val names = KNOWN_APPS.filter { (pkg, _) -> Prefs.enabled(this, pkg) && installed(pkg) }.values
        Hub.link?.send(JSONObject().put("t", "apps").put("list", JSONArray(names.toList())))
    }

    private fun installed(pkg: String) = try {
        packageManager.getPackageInfo(pkg, 0); true
    } catch (e: Exception) {
        false
    }

    override fun onNotificationPosted(sbn: StatusBarNotification) {
        val pkg = sbn.packageName
        val app = KNOWN_APPS[pkg] ?: return
        if (!Prefs.enabled(this, pkg)) return
        val n = sbn.notification
        if (n.flags and Notification.FLAG_GROUP_SUMMARY != 0) return
        val ex = n.extras
        val style = NotificationCompat.MessagingStyle.extractMessagingStyleFromNotification(n)
        val title = ex.getCharSequence(Notification.EXTRA_TITLE)?.toString() ?: return
        val conv = style?.conversationTitle?.toString() ?: title
        val group = style?.isGroupConversation ?: false
        val chat = "$pkg|$conv"
        latest[chat] = sbn
        val messages = style?.messages?.map { m ->
            // A message without a sender is the user's own.
            Triple(m.person?.name?.toString(), m.text?.toString() ?: "", m.timestamp)
        } ?: listOf(Triple(title, ex.getCharSequence(Notification.EXTRA_TEXT)?.toString() ?: "", sbn.postTime))
        for ((who, text, ts) in messages) {
            if (who == null || text.isBlank()) continue
            val id = hash("$chat|$who|$ts|$text")
            if (seen.put(id, true) != null) continue
            Hub.link?.send(JSONObject().put("t", "msg").put("chat", chat).put("app", app).put("conv", conv).put("group", group)
                .put("who", who).put("text", text).put("ts", ts).put("id", id))
        }
    }

    private fun hash(s: String): String =
        MessageDigest.getInstance("SHA-256").digest(s.toByteArray()).take(12).joinToString("") { "%02x".format(it) }

    private fun fromComputer(m: JSONObject) {
        val chat = m.optString("chat")
        when (m.optString("t")) {
            "reply" -> {
                val ok = reply(chat, m.optString("text"))
                Hub.link?.send(JSONObject().put("t", if (ok == null) "sent" else "failed").put("chat", chat).put("ref", m.optString("ref")).put("why", ok ?: ""))
            }
            "read" -> markRead(chat)
        }
    }

    /** Answer through the notification's reply button; null when it worked, else why not. */
    private fun reply(chat: String, text: String): String? {
        val sbn = latest[chat] ?: return "no notification of this chat on the phone any more; answer there"
        val action = sbn.notification.actions?.firstOrNull { a -> a.remoteInputs?.any { it.allowFreeFormInput } == true }
            ?: return "this notification has no reply button"
        return try {
            val intent = Intent()
            val results = Bundle()
            action.remoteInputs.forEach { results.putCharSequence(it.resultKey, text) }
            RemoteInput.addResultsToIntent(action.remoteInputs, intent, results)
            action.actionIntent.send(this, 0, intent)
            null
        } catch (e: Exception) {
            "the app did not take it: ${e.message}"
        }
    }

    private fun markRead(chat: String) {
        val sbn = latest[chat] ?: return
        sbn.notification.actions?.firstOrNull { it.semanticAction == Notification.Action.SEMANTIC_ACTION_MARK_AS_READ }?.let {
            try {
                it.actionIntent.send()
            } catch (e: Exception) {
            }
        }
    }
}
