package dev.vegameta.rawzero.probe

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.graphics.Typeface
import android.os.Bundle
import android.widget.ScrollView
import android.widget.TextView
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * Camera access probe: lists every camera ID the framework lets this package see, then
 * captures one RAW frame per RAW-capable camera (and per physical member of logical
 * cameras) into getExternalFilesDir()/probe-<time>/ together with report.txt.
 *
 * Start with: adb shell am start -n <package>/dev.vegameta.rawzero.probe.ProbeActivity
 * Extra `--ez capture false` lists cameras without capturing.
 */
class ProbeActivity : Activity() {
    private val scope = MainScope()
    private lateinit var text: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        text = TextView(this).apply {
            typeface = Typeface.MONOSPACE
            textSize = 9f
            setTextIsSelectable(true)
            setPadding(16, 16, 16, 16)
        }
        setContentView(ScrollView(this).apply { addView(text) })
        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            start()
        } else {
            requestPermissions(arrayOf(Manifest.permission.CAMERA), 1)
        }
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        if (results.firstOrNull() == PackageManager.PERMISSION_GRANTED) start() else text.text = "camera permission denied"
    }

    private fun start() {
        val stamp = SimpleDateFormat("yyyyMMdd-HHmmss", Locale.US).format(Date())
        val out = File(getExternalFilesDir(null), "probe-$stamp").apply { mkdirs() }
        val capture = intent.getBooleanExtra("capture", true)
        scope.launch {
            Probe(this@ProbeActivity, out) { line -> text.append(line + "\n") }.run(capture)
        }
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }
}
