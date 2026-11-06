package dev.vegameta.rawzero

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.hardware.camera2.CameraManager
import android.os.Bundle
import android.util.Log
import android.view.Gravity
import android.view.OrientationEventListener
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.TextView
import dev.vegameta.rawzero.camera.CameraCatalog
import dev.vegameta.rawzero.camera.CaptureSession
import dev.vegameta.rawzero.camera.Lens
import dev.vegameta.rawzero.camera.ShotSaver
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext

private const val TAG = "RawZero"
private const val PREF_RAW = "save_dng"
private const val PREF_DEV = "save_dev_pairs"

/**
 * Capture tool (milestone 2): ISP preview, every discovered lens, shutter saves
 * DNG + ISP JPEG pairs to Pictures/RawZero/dev/ for desktop look development.
 */
class CaptureActivity : Activity(), SurfaceHolder.Callback {
    private val scope = MainScope()
    private val cameraLock = Mutex()
    private lateinit var cm: CameraManager
    private lateinit var surface: SurfaceView
    private lateinit var lensBar: LinearLayout
    private lateinit var status: TextView
    private lateinit var shutter: Button
    private var lenses: List<Lens> = emptyList()
    private var current: Lens? = null
    private var session: CaptureSession? = null
    private var surfaceReady = false
    private var deviceDegrees = 0
    private var openJob: Job? = null
    private lateinit var orientation: OrientationEventListener

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        cm = getSystemService(CameraManager::class.java)
        buildUi()
        orientation = object : OrientationEventListener(this) {
            override fun onOrientationChanged(o: Int) {
                if (o != ORIENTATION_UNKNOWN) deviceDegrees = o
            }
        }
        if (checkSelfPermission(Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.CAMERA), 1)
        } else {
            discover()
        }
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        if (results.firstOrNull() == PackageManager.PERMISSION_GRANTED) discover() else status.text = "нет доступа к камере"
    }

    private fun buildUi() {
        // HyperOS force-darkens apps, which turned the white shutter black and hid the text.
        val root = FrameLayout(this).apply { setBackgroundColor(Color.BLACK); isForceDarkAllowed = false }
        surface = SurfaceView(this).also { it.holder.addCallback(this) }
        root.addView(surface, FrameLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT, Gravity.TOP))
        val bottom = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            setPadding(0, 16, 0, 48)
        }
        status = TextView(this).apply { setTextColor(Color.LTGRAY); textSize = 13f; gravity = Gravity.CENTER }
        lensBar = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        shutter = Button(this).apply {
            background = GradientDrawable().apply { shape = GradientDrawable.OVAL; setColor(Color.WHITE); setStroke(8, Color.GRAY) }
            setOnClickListener { shoot() }
        }
        bottom.addView(status, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        bottom.addView(HorizontalScrollView(this).apply { addView(lensBar) }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { topMargin = 16 })
        val shutterRow = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            addView(toggle("RAW", PREF_RAW, false), LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
            addView(shutter, LinearLayout.LayoutParams(200, 200).apply { leftMargin = 64; rightMargin = 64 })
            addView(toggle("dev", PREF_DEV, true), LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        }
        bottom.addView(shutterRow, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { topMargin = 32 })
        root.addView(bottom, FrameLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT, Gravity.BOTTOM))
        // Keep controls clear of the status and navigation bars.
        root.setOnApplyWindowInsetsListener { v, insets ->
            val bars = insets.getInsets(android.view.WindowInsets.Type.systemBars())
            v.setPadding(0, bars.top, 0, bars.bottom)
            insets
        }
        setContentView(root)
    }

    private fun discover() {
        scope.launch {
            lenses = withContext(Dispatchers.Default) { CameraCatalog.discover(cm) }
            Log.i(TAG, "lenses: " + lenses.joinToString { "${it.label}=${it.id}${if (it.listed) "" else "(hidden)"}" })
            lensBar.removeAllViews()
            for (l in lenses) {
                lensBar.addView(Button(this@CaptureActivity).apply {
                    text = l.label
                    isAllCaps = false
                    setOnClickListener { select(l) }
                })
            }
            // `adb shell am start ... --es lens <id>` preselects a lens (testing).
            val wanted = intent.getStringExtra("lens")?.let { id -> lenses.firstOrNull { it.id == id } }
            if (lenses.isEmpty()) status.text = "RAW-камер не найдено"
            else select(wanted ?: lenses.firstOrNull { !it.front && it.eqMm in 20..30 } ?: lenses.first())
        }
    }

    private fun select(lens: Lens) {
        current = lens
        for (i in 0 until lensBar.childCount) {
            (lensBar.getChildAt(i) as Button).alpha = if (lenses[i] == lens) 1f else 0.5f
        }
        // Portrait UI: the 4:3 landscape sensor frame shows as 3:4.
        val size = CaptureSession.previewSize(lens)
        surface.holder.setFixedSize(size.width, size.height)
        val w = resources.displayMetrics.widthPixels
        surface.layoutParams = surface.layoutParams.apply { width = w; height = w * size.width / size.height }
        reopen()
    }

    private fun reopen() {
        val lens = current ?: return
        if (!surfaceReady) return
        openJob?.cancel()
        openJob = scope.launch {
            cameraLock.withLock {
                session?.close()
                session = null
                try {
                    session = CaptureSession.open(cm, lens, surface.holder.surface)
                    status.text = "${lens.label}  (id ${lens.id})"
                    Log.i(TAG, "opened ${lens.id}")
                } catch (e: Throwable) {
                    Log.e(TAG, "open ${lens.id}", e)
                    status.text = "ошибка камеры ${lens.id}: ${e.message}"
                }
            }
        }
    }

    private fun shoot() {
        val s = session ?: return
        shutter.isEnabled = false
        status.text = "${s.lens.label}  обработка…"
        scope.launch {
            try {
                val msg = cameraLock.withLock {
                    val shot = s.shoot(ShotSaver.jpegOrientation(s.lens.chars, deviceDegrees))
                    shot.use { withContext(Dispatchers.Default) { process(it) } }
                }
                status.text = msg
            } catch (e: Throwable) {
                Log.e(TAG, "shoot", e)
                status.text = "ошибка съёмки: ${e.message}"
            } finally {
                shutter.isEnabled = true
            }
        }
    }

    /** Our pipeline -> gallery JPEG; optional DNG; dev pairs (DNG + ISP JPEG) for look work. */
    private fun process(shot: dev.vegameta.rawzero.camera.Shot): String {
        val name = ShotSaver.baseName(shot)
        val t0 = System.nanoTime()
        val jpeg = Native.process(shot.raw, shot.lens.chars, shot.result, ShotSaver.exifOrientation(shot.jpegOrientation))
        val ms = (System.nanoTime() - t0) / 1_000_000
        val saved = mutableListOf(ShotSaver.saveJpeg(contentResolver, ShotSaver.DIR, name, jpeg))
        if (prefs.getBoolean(PREF_DEV, true)) {
            saved += ShotSaver.saveDng(contentResolver, ShotSaver.DIR_DEV, name, shot)
            saved += ShotSaver.saveJpeg(contentResolver, ShotSaver.DIR_DEV, name + "_isp", shot.ispJpeg)
        } else if (prefs.getBoolean(PREF_RAW, false)) {
            saved += ShotSaver.saveDng(contentResolver, ShotSaver.DIR_DNG, name, shot)
        }
        Log.i(TAG, "saved $saved pipeline ${ms}ms ${ShotSaver.summary(shot.result)}")
        return "${shot.lens.label}  ${ShotSaver.summary(shot.result)}  ${ms} мс"
    }

    private val prefs by lazy { getSharedPreferences("rawzero", MODE_PRIVATE) }

    private fun toggle(label: String, key: String, default: Boolean) = Button(this).apply {
        isAllCaps = false
        fun refresh() {
            val on = prefs.getBoolean(key, default)
            text = label
            alpha = if (on) 1f else 0.4f
        }
        refresh()
        setOnClickListener {
            prefs.edit().putBoolean(key, !prefs.getBoolean(key, default)).apply()
            refresh()
        }
    }

    /** Volume keys and the camera key act as shutter (also lets adb trigger shots). */
    override fun onKeyDown(keyCode: Int, event: android.view.KeyEvent): Boolean = when (keyCode) {
        android.view.KeyEvent.KEYCODE_VOLUME_DOWN, android.view.KeyEvent.KEYCODE_VOLUME_UP, android.view.KeyEvent.KEYCODE_CAMERA -> {
            if (event.repeatCount == 0 && shutter.isEnabled) shoot()
            true
        }
        else -> super.onKeyDown(keyCode, event)
    }

    override fun surfaceCreated(h: SurfaceHolder) {}
    override fun surfaceChanged(h: SurfaceHolder, format: Int, w: Int, hgt: Int) {
        val lens = current ?: return
        val want = CaptureSession.previewSize(lens)
        if (w == want.width && hgt == want.height) {
            surfaceReady = true
            if (session?.lens != lens) reopen()
        }
    }
    override fun surfaceDestroyed(h: SurfaceHolder) {
        surfaceReady = false
        closeCamera()
    }

    override fun onResume() {
        super.onResume()
        orientation.enable()
        if (surfaceReady && session == null) reopen()
    }

    override fun onPause() {
        orientation.disable()
        closeCamera()
        super.onPause()
    }

    private fun closeCamera() {
        openJob?.cancel()
        session?.close()
        session = null
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }
}
