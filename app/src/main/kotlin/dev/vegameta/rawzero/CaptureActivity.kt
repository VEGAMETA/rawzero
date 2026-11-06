package dev.vegameta.rawzero

import android.Manifest
import android.animation.Animator
import android.animation.AnimatorListenerAdapter
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Color
import android.graphics.Matrix
import android.graphics.Outline
import android.graphics.drawable.GradientDrawable
import android.hardware.camera2.CameraManager
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.util.Size
import android.view.Gravity
import android.view.KeyEvent
import android.view.OrientationEventListener
import android.view.PixelCopy
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.ViewOutlineProvider
import android.view.WindowInsets
import android.view.animation.DecelerateInterpolator
import android.widget.Button
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.SeekBar
import android.widget.TextView
import dev.vegameta.rawzero.camera.CameraCatalog
import dev.vegameta.rawzero.camera.CaptureSession
import dev.vegameta.rawzero.camera.Controls
import dev.vegameta.rawzero.camera.Lens
import dev.vegameta.rawzero.camera.LensOptions
import dev.vegameta.rawzero.camera.Shot
import dev.vegameta.rawzero.camera.ShotSaver
import dev.vegameta.rawzero.camera.formatExposure
import dev.vegameta.rawzero.camera.formatFocus
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext

private const val TAG = "RawZero"
private const val PREF_MODE = "mode"
private const val PREF_DEV = "save_dev_pairs"
private const val PREF_LENS = "lens"
private const val PREF_FLASH = "flash"
/// Exposures from this length on get the ring + countdown indicator.
private const val LONG_EXPOSURE_UI_NS = 500_000_000L

/**
 * NIGHT: burst merge (not implemented yet: single Classic frame). CLASSIC: our pipeline with
 * raw noise reduction and tone/WB matched to the preview. RAW: Process Zero + DNG.
 * SCREEN: exactly what the screen showed - the ISP's own JPEG of the shot.
 */
enum class Mode(val title: String) { NIGHT("Ночь"), CLASSIC("Цифровой"), RAW("RAW"), SCREEN("Экран") }

/**
 * Camera screen: ISP preview, lens bar, modes, manual controls (all auto by default),
 * shutter with a fly-to-thumbnail animation, last-photo thumbnail opening the gallery.
 */
class CaptureActivity : Activity(), SurfaceHolder.Callback {
    private val scope = MainScope()
    private val cameraLock = Mutex()
    private val prefs by lazy { getSharedPreferences("rawzero", MODE_PRIVATE) }
    private lateinit var cm: CameraManager
    private lateinit var root: FrameLayout
    private lateinit var previewBox: FrameLayout
    private lateinit var surface: SurfaceView
    private lateinit var flash: View
    private lateinit var panel: LinearLayout
    private lateinit var lensBar: LinearLayout
    private lateinit var modeBar: LinearLayout
    private lateinit var status: TextView
    private lateinit var shutter: View
    private lateinit var gear: Button
    private lateinit var thumb: ImageView
    private lateinit var ring: dev.vegameta.rawzero.ui.ProgressRing
    private lateinit var countdown: TextView
    private var exposureAnim: android.animation.ValueAnimator? = null
    private lateinit var zoomLabel: TextView
    private lateinit var flashButton: TextView
    private var flashMode: dev.vegameta.rawzero.camera.Flash
        get() = runCatching { dev.vegameta.rawzero.camera.Flash.valueOf(prefs.getString(PREF_FLASH, "OFF")!!) }.getOrDefault(dev.vegameta.rawzero.camera.Flash.OFF)
        set(v) = prefs.edit().putString(PREF_FLASH, v.name).apply()
    /** Preview frame grabbed at the shutter: look reference when the lens has no ISP JPEG. */
    @Volatile private var shutterFrame: Bitmap? = null
    private lateinit var orientation: OrientationEventListener

    private var lenses: List<Lens> = emptyList()
    private var current: Lens? = null
    private var session: CaptureSession? = null
    private var controls = Controls()
    private var surfaceReady = false
    private var deviceDegrees = 0
    private var openJob: Job? = null
    private var lastPhoto: Uri? = null
    private var busy = false
    private var mode: Mode
        get() = runCatching { Mode.valueOf(prefs.getString(PREF_MODE, Mode.CLASSIC.name)!!) }.getOrDefault(Mode.CLASSIC)
        set(v) = prefs.edit().putString(PREF_MODE, v.name).apply()

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
        loadLastThumbnail()
        // Testing: `--es mode CLASSIC|NIGHT|RAW|SCREEN`.
        intent.getStringExtra("mode")?.let { runCatching { mode = Mode.valueOf(it); refreshModes() } }
        if (intent.hasExtra("dev")) prefs.edit().putBoolean(PREF_DEV, intent.getBooleanExtra("dev", false)).apply()
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        if (results.firstOrNull() == PackageManager.PERMISSION_GRANTED) discover() else status.text = "нет доступа к камере"
    }

    // ---------------------------------------------------------------- UI

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun buildUi() {
        // HyperOS force-darkens apps, which turned the white shutter black and hid the text.
        root = FrameLayout(this).apply { setBackgroundColor(Color.BLACK); isForceDarkAllowed = false }
        val column = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(column, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))

        previewBox = FrameLayout(this)
        surface = SurfaceView(this).also { it.holder.addCallback(this) }
        previewBox.addView(surface, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
        flash = View(this).apply { setBackgroundColor(Color.BLACK); alpha = 0f }
        previewBox.addView(flash, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
        panel = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.argb(170, 0, 0, 0))
            setPadding(dp(16), dp(8), dp(16), dp(8))
            visibility = View.GONE
        }
        previewBox.addView(panel, FrameLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT, Gravity.BOTTOM))
        countdown = TextView(this).apply {
            setTextColor(Color.WHITE)
            textSize = 56f
            gravity = Gravity.CENTER
            setShadowLayer(12f, 0f, 0f, Color.BLACK)
            setBackgroundColor(Color.argb(90, 0, 0, 0))
            visibility = View.GONE
        }
        previewBox.addView(countdown, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
        zoomLabel = TextView(this).apply {
            setTextColor(Color.WHITE)
            textSize = 13f
            setPadding(dp(10), dp(4), dp(10), dp(4))
            background = GradientDrawable().apply { cornerRadius = dp(14).toFloat(); setColor(Color.argb(110, 0, 0, 0)) }
            setOnClickListener { setZoom(1f) }
        }
        previewBox.addView(zoomLabel, FrameLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT, Gravity.TOP or Gravity.CENTER_HORIZONTAL).apply { topMargin = dp(12) })
        flashButton = TextView(this).apply {
            setTextColor(Color.WHITE)
            textSize = 13f
            setPadding(dp(12), dp(6), dp(12), dp(6))
            background = GradientDrawable().apply { cornerRadius = dp(16).toFloat(); setColor(Color.argb(110, 0, 0, 0)) }
            setOnClickListener {
                val all = dev.vegameta.rawzero.camera.Flash.entries
                flashMode = all[(flashMode.ordinal + 1) % all.size]
                refreshFlash()
                update(controls.copy(flash = flashMode))
            }
        }
        previewBox.addView(flashButton, FrameLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT, Gravity.TOP or Gravity.START).apply { topMargin = dp(10); leftMargin = dp(10) })
        installZoomGestures()
        column.addView(previewBox, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))

        column.addView(View(this), LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        status = TextView(this).apply { setTextColor(Color.LTGRAY); textSize = 12f; gravity = Gravity.CENTER }
        column.addView(status, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        lensBar = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL; gravity = Gravity.CENTER }
        column.addView(HorizontalScrollView(this).apply { addView(lensBar); isHorizontalScrollBarEnabled = false },
            LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { gravity = Gravity.CENTER_HORIZONTAL; topMargin = dp(6) })
        modeBar = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL; gravity = Gravity.CENTER }
        for (m in Mode.entries) {
            modeBar.addView(TextView(this).apply {
                text = m.title
                textSize = 15f
                setPadding(dp(14), dp(10), dp(14), dp(10))
                setOnClickListener { mode = m; refreshModes() }
            })
        }
        column.addView(modeBar, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { topMargin = dp(4) })

        val row = FrameLayout(this)
        gear = Button(this).apply {
            text = "AUTO"
            textSize = 11f
            isAllCaps = false
            setTextColor(Color.WHITE)
            background = GradientDrawable().apply { shape = GradientDrawable.OVAL; setColor(Color.argb(60, 255, 255, 255)) }
            setOnClickListener { panel.visibility = if (panel.visibility == View.VISIBLE) View.GONE else View.VISIBLE }
        }
        row.addView(gear, FrameLayout.LayoutParams(dp(56), dp(56), Gravity.START or Gravity.CENTER_VERTICAL).apply { leftMargin = dp(32) })
        shutter = View(this).apply {
            background = GradientDrawable().apply { shape = GradientDrawable.OVAL; setColor(Color.WHITE); setStroke(dp(5), Color.GRAY) }
            setOnClickListener { shoot() }
        }
        row.addView(shutter, FrameLayout.LayoutParams(dp(76), dp(76), Gravity.CENTER))
        ring = dev.vegameta.rawzero.ui.ProgressRing(this).apply { visibility = View.GONE }
        row.addView(ring, FrameLayout.LayoutParams(dp(92), dp(92), Gravity.CENTER))
        thumb = ImageView(this).apply {
            scaleType = ImageView.ScaleType.CENTER_CROP
            background = GradientDrawable().apply { cornerRadius = dp(12).toFloat(); setColor(Color.DKGRAY) }
            outlineProvider = object : ViewOutlineProvider() {
                override fun getOutline(v: View, o: Outline) = o.setRoundRect(0, 0, v.width, v.height, dp(12).toFloat())
            }
            clipToOutline = true
            setOnClickListener { openGallery() }
        }
        row.addView(thumb, FrameLayout.LayoutParams(dp(56), dp(56), Gravity.END or Gravity.CENTER_VERTICAL).apply { rightMargin = dp(32) })
        column.addView(row, LinearLayout.LayoutParams(MATCH_PARENT, dp(100)).apply { topMargin = dp(4); bottomMargin = dp(12) })

        root.setOnApplyWindowInsetsListener { v, insets ->
            val bars = insets.getInsets(WindowInsets.Type.systemBars())
            v.setPadding(0, bars.top, 0, bars.bottom)
            insets
        }
        setContentView(root)
        refreshModes()
    }

    private fun refreshModes() {
        for (i in 0 until modeBar.childCount) {
            val t = modeBar.getChildAt(i) as TextView
            val on = Mode.entries[i] == mode
            t.setTextColor(if (on) Color.rgb(255, 204, 0) else Color.LTGRAY)
            t.paint.isFakeBoldText = on
            t.invalidate()
        }
    }

    /** Settings sliders; index 0 of every slider except EV is "auto". */
    private fun buildPanel(lens: Lens) {
        panel.removeAllViews()
        val o = LensOptions(lens)
        Log.i(TAG, "options ${lens.key}: shutter up to ${o.exposures.lastOrNull()?.let(::formatExposure)}, ISO up to ${o.isos.lastOrNull()}")
        fun row(label: String, count: Int, initial: Int, fmt: (Int) -> String, set: (Int) -> Unit) {
            val value = TextView(this).apply { setTextColor(Color.WHITE); textSize = 13f; text = fmt(initial) }
            val head = LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                addView(TextView(this@CaptureActivity).apply { text = label; setTextColor(Color.LTGRAY); textSize = 13f },
                    LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
                addView(value)
            }
            val bar = SeekBar(this).apply {
                max = count - 1
                progress = initial
                setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
                    override fun onProgressChanged(s: SeekBar, p: Int, fromUser: Boolean) {
                        if (!fromUser) return
                        value.text = fmt(p)
                        set(p)
                    }
                    override fun onStartTrackingTouch(s: SeekBar) {}
                    override fun onStopTrackingTouch(s: SeekBar) {}
                })
            }
            panel.addView(head)
            panel.addView(bar)
        }
        val evCount = o.evRange.last - o.evRange.first + 1
        if (evCount > 1) {
            row("Экспозиция", evCount, controls.evSteps - o.evRange.first,
                { "%+.1f EV".format((it + o.evRange.first) * o.evStep) }) { update(controls.copy(evSteps = it + o.evRange.first)) }
        }
        row("Выдержка", o.exposures.size, o.exposures.indexOf(controls.exposureNs).coerceAtLeast(0),
            { o.exposures[it]?.let(::formatExposure) ?: "авто" }) { update(controls.copy(exposureNs = o.exposures[it])) }
        row("ISO", o.isos.size, o.isos.indexOf(controls.iso).coerceAtLeast(0),
            { o.isos[it]?.toString() ?: "авто" }) { update(controls.copy(iso = o.isos[it])) }
        if (o.minFocus > 0f) {
            row("Фокус", o.focusSteps + 1, 0, { o.focusAt(it)?.let(::formatFocus) ?: "авто" }) { update(controls.copy(focusDiopters = o.focusAt(it))) }
        }
        row("Баланс белого", o.kelvins.size, o.kelvins.indexOf(controls.kelvin).coerceAtLeast(0),
            { o.kelvins[it]?.let { k -> "$k K" } ?: "авто" }) { update(controls.copy(kelvin = o.kelvins[it])) }

        val bottom = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL; gravity = Gravity.CENTER_VERTICAL }
        bottom.addView(Button(this).apply {
            text = "Всё авто"
            isAllCaps = false
            setOnClickListener { update(Controls(zoom = controls.zoom, flash = flashMode)); buildPanel(lens) }
        })
        bottom.addView(Button(this).apply {
            isAllCaps = false
            fun refresh() { text = if (prefs.getBoolean(PREF_DEV, false)) "dev-пары: вкл" else "dev-пары: выкл" }
            refresh()
            setOnClickListener { prefs.edit().putBoolean(PREF_DEV, !prefs.getBoolean(PREF_DEV, false)).apply(); refresh() }
        }, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT).apply { leftMargin = dp(12) })
        panel.addView(bottom)
        panel.addView(Button(this).apply {
            text = "Пересканировать камеры"
            isAllCaps = false
            setOnClickListener { panel.visibility = View.GONE; discover(rescan = true) }
        })
    }

    private fun refreshFlash() {
        flashButton.text = when (flashMode) {
            dev.vegameta.rawzero.camera.Flash.OFF -> "⚡ выкл"
            dev.vegameta.rawzero.camera.Flash.CAPTURE -> "⚡ при съёмке"
            dev.vegameta.rawzero.camera.Flash.ALWAYS -> "⚡ всегда"
        }
        flashButton.alpha = if (flashMode == dev.vegameta.rawzero.camera.Flash.OFF) 0.6f else 1f
        flashButton.visibility = if (current?.let { dev.vegameta.rawzero.camera.hasFlash(it.chars) } == true) View.VISIBLE else View.GONE
    }

    private fun maxZoom(): Float = current?.let { dev.vegameta.rawzero.camera.zoomRange(it.chars).first } ?: 1f

    private fun setZoom(z: Float) {
        val zz = z.coerceIn(1f, maxZoom())
        zoomLabel.text = if (zz < 9.95f) "%.1f×".format(zz) else "%.0f×".format(zz)
        if (zz != controls.zoom) update(controls.copy(zoom = zz))
    }

    /** Pinch to zoom on the preview, double tap toggles 1x / 2x. */
    @android.annotation.SuppressLint("ClickableViewAccessibility")
    private fun installZoomGestures() {
        val scale = android.view.ScaleGestureDetector(this, object : android.view.ScaleGestureDetector.SimpleOnScaleGestureListener() {
            override fun onScale(d: android.view.ScaleGestureDetector): Boolean {
                setZoom(controls.zoom * d.scaleFactor)
                return true
            }
        })
        val taps = android.view.GestureDetector(this, object : android.view.GestureDetector.SimpleOnGestureListener() {
            override fun onDoubleTap(e: android.view.MotionEvent): Boolean {
                setZoom(if (controls.zoom > 1.01f) 1f else 2f)
                return true
            }
        })
        surface.setOnTouchListener { _, ev ->
            scale.onTouchEvent(ev)
            taps.onTouchEvent(ev)
            true
        }
    }

    private fun update(c: Controls) {
        controls = c
        gear.text = if (c.copy(zoom = 1f, flash = dev.vegameta.rawzero.camera.Flash.OFF) == Controls()) "AUTO" else "M"
        val s = session ?: return
        runCatching { s.setControls(c) }.onFailure { Log.e(TAG, "controls", it) }
    }

    // ---------------------------------------------------------------- camera

    /**
     * Lenses from the per-build cache, or a full scan (first launch, OS update, or [rescan]):
     * every candidate is opened and configured once, which takes a few seconds.
     */
    private fun discover(rescan: Boolean = false) {
        scope.launch {
            closeCamera()
            val cached = if (rescan) null else withContext(Dispatchers.Default) { CameraCatalog.cached(this@CaptureActivity, cm) }
            lenses = cached ?: cameraLock.withLock {
                CameraCatalog.scan(this@CaptureActivity, cm) { done, total ->
                    withContext(Dispatchers.Main) { status.text = "ищу камеры… $done из $total" }
                }
            }
            Log.i(TAG, "lenses: " + lenses.joinToString { "${it.label}=${it.key}${if (it.listed) "" else "(hidden)"}" })
            lensBar.removeAllViews()
            for (l in lenses) {
                lensBar.addView(Button(this@CaptureActivity).apply {
                    text = l.label
                    isAllCaps = false
                    textSize = 13f
                    setTextColor(Color.WHITE)
                    background = GradientDrawable().apply { cornerRadius = dp(18).toFloat(); setColor(Color.argb(50, 255, 255, 255)) }
                    setOnClickListener { select(l) }
                }, LinearLayout.LayoutParams(WRAP_CONTENT, dp(36)).apply { leftMargin = dp(4); rightMargin = dp(4) })
            }
            // `adb shell am start ... --es lens <id>` preselects a lens (testing).
            // The lens from an adb test extra, else the one used last time.
            val wantedKey = intent.getStringExtra("lens") ?: prefs.getString(PREF_LENS, null)
            val wanted = wantedKey?.let { id -> lenses.firstOrNull { it.id == id || it.key == id } }
            if (android.os.Build.MANUFACTURER.equals("samsung", ignoreCase = true) && packageName == "dev.vegameta.rawzero") {
                // Samsung shows its telephotos only to allow-listed package names.
                status.text = "на Samsung установите «RawZero S», чтобы открылись телевики"
            }
            if (lenses.isEmpty()) status.text = "камер не найдено"
            else select(wanted ?: lenses.firstOrNull { !it.front && it.eqMm in 20..30 } ?: lenses.first())
            // Test presets: --ef zoom <x> --es flash OFF|CAPTURE|ALWAYS
            intent.getStringExtra("flash")?.let { runCatching { flashMode = dev.vegameta.rawzero.camera.Flash.valueOf(it); refreshFlash(); update(controls.copy(flash = flashMode)) } }
            if (intent.hasExtra("zoom")) setZoom(intent.getFloatExtra("zoom", 1f))
            // Test presets after the lens is chosen: --el exp <ns> --ei iso <n> --ei kelvin <K> --ei ev <steps>.
            if (intent.hasExtra("exp") || intent.hasExtra("iso") || intent.hasExtra("kelvin") || intent.hasExtra("ev")) {
                update(Controls(
                    evSteps = intent.getIntExtra("ev", 0),
                    exposureNs = intent.getLongExtra("exp", 0L).takeIf { it > 0 },
                    iso = intent.getIntExtra("iso", 0).takeIf { it > 0 },
                    kelvin = intent.getIntExtra("kelvin", 0).takeIf { it > 0 },
                ))
                current?.let(::buildPanel)
            }
        }
    }

    private fun select(lens: Lens) {
        current = lens
        for (i in 0 until lensBar.childCount) {
            lensBar.getChildAt(i).alpha = if (lenses[i] == lens) 1f else 0.45f
        }
        // Lens ranges differ: keep only exposure compensation across a lens switch.
        prefs.edit().putString(PREF_LENS, lens.key).apply()
        update(Controls(evSteps = controls.evSteps, flash = flashMode))
        setZoom(1f)
        refreshFlash()
        buildPanel(lens)
        // Portrait UI: the 4:3 landscape sensor frame shows as 3:4.
        val size = CaptureSession.previewSize(lens)
        surface.holder.setFixedSize(size.width, size.height)
        val w = resources.displayMetrics.widthPixels
        previewBox.layoutParams = previewBox.layoutParams.apply { width = w; height = w * size.width / size.height }
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
                    session = CaptureSession.open(cm, lens, surface.holder.surface, controls)
                    status.text = lens.label
                    Log.i(TAG, "opened ${lens.id}")
                } catch (e: kotlinx.coroutines.CancellationException) {
                    throw e // superseded by a newer reopen (lens switch), not an error
                } catch (e: Throwable) {
                    Log.e(TAG, "open ${lens.key}", e)
                    status.text = "ошибка камеры ${lens.label}: ${e.message}"
                }
            }
        }
    }

    private fun shoot() {
        val s = session ?: return
        if (busy) return
        busy = true
        shutter.alpha = 0.5f
        val m = mode
        shutterFrame = null
        animateCapture()
        val longExp = controls.exposureNs?.takeIf { it >= LONG_EXPOSURE_UI_NS }
        status.text = when {
            longExp != null -> "держите телефон неподвижно"
            m == Mode.NIGHT -> "ночь: держите телефон неподвижно…"
            else -> "обработка…"
        }
        longExp?.let(::startExposureIndicator)
        scope.launch {
            try {
                val (msg, uri, bmp) = if (m == Mode.NIGHT && s.lens.raw) cameraLock.withLock { night(s) } else cameraLock.withLock {
                    val shot = s.shoot(ShotSaver.jpegOrientation(s.lens.chars, deviceDegrees))
                    if (longExp != null) exposureDone()
                    shot.use { withContext(Dispatchers.Default) { process(it, m) } }
                }
                lastPhoto = uri
                bmp?.let { thumb.setImageBitmap(it) }
                status.text = msg
            } catch (e: Throwable) {
                Log.e(TAG, "shoot", e)
                status.text = "ошибка съёмки: ${e.message}"
            } finally {
                stopExposureIndicator()
                busy = false
                shutter.alpha = 1f
            }
        }
    }

    /**
     * Long exposure feedback: the preview freezes while the sensor integrates, so a ring
     * around the shutter fills up and the frame shows a countdown of the remaining seconds.
     */
    private fun startExposureIndicator(exposureNs: Long) {
        val ms = exposureNs / 1_000_000
        ring.progress = 0f
        ring.visibility = View.VISIBLE
        countdown.visibility = View.VISIBLE
        exposureAnim?.cancel()
        exposureAnim = android.animation.ValueAnimator.ofFloat(0f, 1f).apply {
            duration = ms
            interpolator = android.view.animation.LinearInterpolator()
            addUpdateListener {
                val p = it.animatedValue as Float
                ring.progress = p
                val left = (ms * (1 - p)) / 1000.0
                countdown.text = if (left >= 10) "%.0f".format(left) else "%.1f".format(left)
            }
            start()
        }
    }

    private fun exposureDone() {
        exposureAnim?.cancel()
        ring.progress = 1f
        countdown.textSize = 22f
        countdown.text = "обработка…"
    }

    private fun stopExposureIndicator() {
        exposureAnim?.cancel()
        exposureAnim = null
        ring.visibility = View.GONE
        countdown.visibility = View.GONE
        countdown.textSize = 56f
    }

    /**
     * Night: an 8-frame RAW burst, each frame aligned and merged in Rust as it arrives (HDR+),
     * then the Classic engine with the preview frame as look reference.
     */
    private suspend fun night(s: CaptureSession): Triple<String, Uri, Bitmap?> {
        val orientation = ShotSaver.jpegOrientation(s.lens.chars, deviceDegrees)
        val mirror = if (s.lens.front) orientation else null
        val sensorRot = s.lens.chars[android.hardware.camera2.CameraCharacteristics.SENSOR_ORIENTATION] ?: 0
        var burst: Native.NightBurst? = null
        val t0 = System.nanoTime()
        try {
            s.shootNight(dev.vegameta.rawzero.camera.NIGHT_FRAMES, orientation) { k, img, res ->
                withContext(Dispatchers.Default) {
                    if (k == 0) {
                        burst = Native.NightBurst(img, s.lens.chars, res!!, ShotSaver.exifOrientation(orientation), mirror, sensorRot, s.lens.front,
                            s.controls.zoom.coerceIn(1f, dev.vegameta.rawzero.camera.zoomRange(s.lens.chars).first))
                    } else {
                        burst!!.add(img)
                    }
                }
                withContext(Dispatchers.Main) {
                    status.text = "ночь: кадр ${k + 1} из ${dev.vegameta.rawzero.camera.NIGHT_FRAMES}"
                    ring.visibility = View.VISIBLE
                    ring.progress = (k + 1f) / dev.vegameta.rawzero.camera.NIGHT_FRAMES
                }
            }
            val preview = shutterFrame
            val jpeg = withContext(Dispatchers.Default) { burst!!.finish(preview?.let(Native::referenceRgb)) }
            val ms = (System.nanoTime() - t0) / 1_000_000
            val name = "RZ_" + java.text.SimpleDateFormat("yyyyMMdd_HHmmss_SSS", java.util.Locale.US).format(java.util.Date()) + "_cam" + s.lens.id + "_night"
            val uri = withContext(Dispatchers.IO) { ShotSaver.saveJpeg(contentResolver, ShotSaver.DIR, name, jpeg) }
            Log.i(TAG, "saved $name night ${dev.vegameta.rawzero.camera.NIGHT_FRAMES} frames ${ms}ms")
            return Triple("${s.lens.label}  ночь ${dev.vegameta.rawzero.camera.NIGHT_FRAMES} кадров  $ms мс", uri, thumbnail(jpeg, orientation))
        } finally {
            burst?.close()
        }
    }

    /** Our pipeline -> gallery JPEG; RAW mode adds the DNG; dev pairs keep DNG + ISP JPEG. */
    private fun process(shot: Shot, m: Mode): Triple<String, Uri, Bitmap?> {
        val name = ShotSaver.baseName(shot)
        val raw = shot.raw
        val preview = shutterFrame
        Log.i(TAG, "reference: " + (preview?.let { "preview ${it.width}x${it.height} mean ${meanRgb(it)}" } ?: "none (ISP still)"))
        if (m == Mode.SCREEN && shot.ispJpeg != null) {
            // "As on screen": the ISP still (full detail) recolored to the preview frame the
            // user saw, mirrored for the front lens like the preview.
            val jpeg = dev.vegameta.rawzero.camera.ScreenLook.render(this, shot.ispJpeg, preview, shot.jpegOrientation, shot.lens.front)
            val uri = ShotSaver.saveJpeg(contentResolver, ShotSaver.DIR, name, jpeg)
            Log.i(TAG, "saved $name SCREEN ${ShotSaver.summary(shot.result)}")
            return Triple("${shot.lens.label}  ${ShotSaver.summary(shot.result)}  как на экране", uri, thumbnail(jpeg, shot.jpegOrientation))
        }
        if (raw == null) {
            // ISP-only lens (no RAW in the HAL): keep the phone's own JPEG rather than nothing.
            val isp = requireNotNull(shot.ispJpeg) { "lens ${shot.lens.key} delivered nothing" }
            val jpeg = dev.vegameta.rawzero.camera.ScreenLook.render(this, isp, preview, shot.jpegOrientation, shot.lens.front)
            val uri = ShotSaver.saveJpeg(contentResolver, ShotSaver.DIR, name, jpeg)
            Log.i(TAG, "saved $name ISP-only ${ShotSaver.summary(shot.result)}")
            return Triple("${shot.lens.label}  ${ShotSaver.summary(shot.result)}  без RAW", uri, thumbnail(jpeg, shot.jpegOrientation))
        }
        val t0 = System.nanoTime()
        // Front camera keeps the mirror the preview showed (owner decision).
        val mirror = if (shot.lens.front) shot.jpegOrientation else null
        // Classic / Night: WB, tone and saturation matched to what the user saw - the preview
        // frame grabbed at the shutter (the ISP still is tuned differently), else the ISP still.
        val reference = when {
            m == Mode.RAW -> null
            preview != null -> Native.referenceRgb(preview)
            shot.ispJpeg != null -> Native.referenceRgb(shot.ispJpeg)
            else -> null
        }
        // The preview frame is the sensor image rotated by SENSOR_ORIENTATION (portrait UI) and
        // mirrored for the front lens; the ISP still has the RAW's orientation.
        val previewRef = preview != null && m != Mode.RAW
        val sensorRot = shot.lens.chars[android.hardware.camera2.CameraCharacteristics.SENSOR_ORIENTATION] ?: 0
        val jpeg = Native.process(raw, shot.lens.chars, shot.result, ShotSaver.exifOrientation(shot.jpegOrientation), mirror, reference,
            classic = m != Mode.RAW, refRotateCw = if (previewRef) sensorRot else 0, refMirror = previewRef && shot.lens.front,
            zoom = shot.zoom)
        val ms = (System.nanoTime() - t0) / 1_000_000
        val uri = ShotSaver.saveJpeg(contentResolver, ShotSaver.DIR, name, jpeg)
        if (m == Mode.RAW) ShotSaver.saveDng(contentResolver, ShotSaver.DIR_DNG, name, shot)
        if (prefs.getBoolean(PREF_DEV, false)) {
            ShotSaver.saveDng(contentResolver, ShotSaver.DIR_DEV, name, shot)
            shot.ispJpeg?.let { ShotSaver.saveJpeg(contentResolver, ShotSaver.DIR_DEV, name + "_isp", it) }
            preview?.let { bmp ->
                val o = java.io.ByteArrayOutputStream()
                bmp.compress(Bitmap.CompressFormat.JPEG, 98, o)
                ShotSaver.saveJpeg(contentResolver, ShotSaver.DIR_DEV, name + "_preview", o.toByteArray())
            }
        }
        Log.i(TAG, "saved $name mode $m pipeline ${ms}ms ${ShotSaver.summary(shot.result)}")
        return Triple("${shot.lens.label}  ${ShotSaver.summary(shot.result)}  $ms мс", uri, thumbnail(jpeg, shot.jpegOrientation))
    }

    private fun meanRgb(b: Bitmap): String {
        val px = IntArray(b.width * b.height).also { b.getPixels(it, 0, b.width, 0, 0, b.width, b.height) }
        val s = LongArray(3)
        for (p in px) { s[0] += (p shr 16) and 0xFF; s[1] += (p shr 8) and 0xFF; s[2] += p and 0xFF }
        return s.joinToString("/") { (it / px.size).toString() }
    }

    private fun thumbnail(jpeg: ByteArray, degrees: Int): Bitmap? {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeByteArray(jpeg, 0, jpeg.size, bounds)
        var sample = 1
        while (bounds.outWidth / (sample * 2) >= 256) sample *= 2
        val bmp = BitmapFactory.decodeByteArray(jpeg, 0, jpeg.size, BitmapFactory.Options().apply { inSampleSize = sample }) ?: return null
        if (degrees % 360 == 0) return bmp
        return Bitmap.createBitmap(bmp, 0, 0, bmp.width, bmp.height, Matrix().apply { postRotate(degrees.toFloat()) }, true)
    }

    // ---------------------------------------------------------------- capture animation

    /** Shutter feedback: brief dim, then the current preview frame shrinks into the thumbnail. */
    private fun animateCapture() {
        flash.animate().cancel()
        flash.alpha = 0.6f
        flash.animate().alpha(0f).setDuration(180).start()
        if (surface.width == 0) return
        val bmp = Bitmap.createBitmap(surface.width / 3, surface.height / 3, Bitmap.Config.ARGB_8888)
        PixelCopy.request(surface, bmp, { result ->
            if (result == PixelCopy.SUCCESS) {
                shutterFrame = bmp
                flyToThumbnail(bmp)
            }
        }, Handler(Looper.getMainLooper()))
    }

    private fun flyToThumbnail(bmp: Bitmap) {
        val src = IntArray(2).also { previewBox.getLocationInWindow(it) }
        val dst = IntArray(2).also { thumb.getLocationInWindow(it) }
        val base = IntArray(2).also { root.getLocationInWindow(it) }
        val w = surface.width
        val h = surface.height
        val fly = ImageView(this).apply { setImageBitmap(bmp); scaleType = ImageView.ScaleType.FIT_XY }
        root.addView(fly, FrameLayout.LayoutParams(w, h).apply {
            leftMargin = src[0] - base[0]
            topMargin = src[1] - base[1] - root.paddingTop
        })
        // Scale about the top-left corner so the frame lands centered on the thumbnail.
        fly.pivotX = 0f
        fly.pivotY = 0f
        val scale = thumb.width.toFloat() / w
        val dx = (dst[0] - src[0]).toFloat()
        val dy = (dst[1] - src[1]).toFloat() - (h * scale - thumb.height) / 2f
        fly.animate()
            .translationX(dx).translationY(dy)
            .scaleX(scale).scaleY(scale)
            .alpha(0.85f)
            .setDuration(420)
            .setInterpolator(DecelerateInterpolator(1.6f))
            .setListener(object : AnimatorListenerAdapter() {
                override fun onAnimationEnd(a: Animator) {
                    root.removeView(fly)
                    thumb.setImageBitmap(bmp)
                    thumb.scaleX = 1.15f
                    thumb.scaleY = 1.15f
                    thumb.animate().scaleX(1f).scaleY(1f).setDuration(160).start()
                }
            })
            .start()
    }

    // ---------------------------------------------------------------- gallery

    private fun loadLastThumbnail() {
        scope.launch {
            val found = withContext(Dispatchers.IO) {
                runCatching {
                    ShotSaver.latest(contentResolver)?.let { uri -> uri to contentResolver.loadThumbnail(uri, Size(256, 256), null) }
                }.getOrNull()
            }
            found?.let { (uri, bmp) -> lastPhoto = uri; thumb.setImageBitmap(bmp) }
        }
    }

    private fun openGallery() {
        val uri = lastPhoto ?: return
        runCatching {
            startActivity(Intent(Intent.ACTION_VIEW).setDataAndType(uri, "image/jpeg").addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION))
        }.onFailure { status.text = "нет приложения галереи" }
    }

    // ---------------------------------------------------------------- lifecycle

    /** Volume keys and the camera key act as shutter (also lets adb trigger shots). */
    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean = when (keyCode) {
        KeyEvent.KEYCODE_VOLUME_DOWN, KeyEvent.KEYCODE_VOLUME_UP, KeyEvent.KEYCODE_CAMERA -> {
            if (event.repeatCount == 0) shoot()
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
