package io.github.cyberhirsch.rogueim.phone

import android.content.ComponentName
import android.content.Intent
import android.graphics.Color
import android.graphics.Typeface
import android.os.Bundle
import android.provider.Settings
import android.view.View
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import androidx.activity.ComponentActivity
import androidx.core.app.NotificationManagerCompat
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions

/** One plain screen: link to RogueIM, allow reading notifications, pick the apps. */
class MainActivity : ComponentActivity() {
    private lateinit var status: TextView
    private lateinit var access: Button

    private val scan = registerForActivityResult(ScanContract()) { r -> r.contents?.let { use(it) } }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val fg = Color.rgb(0xd0, 0xd0, 0xc8)
        val dim = Color.rgb(0x80, 0x80, 0x78)
        fun label(t: String, color: Int = fg, size: Float = 14f) = TextView(this).apply {
            text = t; setTextColor(color); textSize = size; typeface = Typeface.MONOSPACE; setPadding(0, 12, 0, 12)
        }
        val col = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(40, 60, 40, 40) }
        col.addView(label("RogueIM Phone", Color.rgb(0x7f, 0xc7, 0xff), 20f))
        status = label("", fg)
        col.addView(status)
        col.addView(Button(this).apply { text = "scan the code from RogueIM"; setOnClickListener { scan.launch(ScanOptions().setPrompt("RogueIM → settings → plugins → Phone").setBeepEnabled(false).setOrientationLocked(false)) } })
        val paste = EditText(this).apply { hint = "or paste the code (rimphone:…)"; setTextColor(fg); setHintTextColor(dim); typeface = Typeface.MONOSPACE; isSingleLine = true }
        col.addView(paste)
        col.addView(Button(this).apply { text = "use pasted code"; setOnClickListener { use(paste.text.toString()) } })
        access = Button(this).apply { setOnClickListener { startActivity(Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)) } }
        col.addView(access)
        col.addView(label("Forward messages from:", fg))
        for ((pkg, name) in KNOWN_APPS) {
            val installed = try { packageManager.getPackageInfo(pkg, 0); true } catch (e: Exception) { false }
            if (!installed) continue
            col.addView(CheckBox(this).apply {
                text = name; setTextColor(fg); typeface = Typeface.MONOSPACE
                isChecked = Prefs.enabled(this@MainActivity, pkg)
                setOnCheckedChangeListener { _, on -> Prefs.setEnabled(this@MainActivity, pkg, on) }
            })
        }
        col.addView(label("Messages go only to your computer, encrypted, over your own network. RogueIM sees what your notifications show; replies go through the apps' reply buttons.", dim, 12f))
        // Android 15 draws apps under the status bar: keep clear of it.
        setContentView(ScrollView(this).apply { setBackgroundColor(Color.rgb(0x07, 0x08, 0x09)); fitsSystemWindows = true; addView(col) })
        Hub.onChange = { runOnUiThread { refresh() } }
    }

    override fun onResume() {
        super.onResume()
        refresh()
    }

    private fun use(code: String) {
        val p = Pairing.parse(code)
        if (p == null) {
            status.text = "That is not a RogueIM code."
            return
        }
        Prefs.setCode(this, code.trim())
        Hub.link?.let { it.pairing = p; it.poke() }
        refresh()
    }

    private fun refresh() {
        val allowed = NotificationManagerCompat.getEnabledListenerPackages(this).contains(packageName)
        access.text = if (allowed) "notification access: allowed" else "allow notification access"
        val p = Prefs.pairing(this)
        val link = Hub.link
        status.text = when {
            p == null -> "Not linked. In RogueIM: settings → plugins → Phone, then scan its code."
            !allowed -> "Linked to ${p.name}. Allow notification access to start."
            link?.connected == true -> "Connected to ${p.name}."
            else -> "Linked to ${p.name}, not connected: same Wi-Fi as the computer? RogueIM running?"
        }
        access.visibility = View.VISIBLE
        if (allowed && link == null) {
            // Ask the system to (re)bind the listener, e.g. right after access was granted.
            android.service.notification.NotificationListenerService.requestRebind(ComponentName(this, Forwarder::class.java))
        }
    }
}
