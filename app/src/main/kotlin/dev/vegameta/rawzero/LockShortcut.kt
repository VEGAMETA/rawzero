package dev.vegameta.rawzero

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.provider.Settings
import org.json.JSONObject

/**
 * The camera button on the HyperOS lock screen. HyperOS keeps its two lock screen shortcuts
 * as JSON in Settings.Secure `keyguard_shortcut_left/right` (written by its lock screen
 * editor, which offers only system apps); the keyguard launches whatever target the JSON
 * names, over the lock screen. Writing it needs WRITE_SECURE_SETTINGS, granted once over adb:
 *   adb shell pm grant <package> android.permission.WRITE_SECURE_SETTINGS
 * Verified on Xiaomi 14, HyperOS 3: the right shortcut opens CaptureActivity without unlock.
 */
object LockShortcut {
    private const val KEY = "keyguard_shortcut_right"
    private const val PREFS = "lock_shortcut"

    /** The stock value on Xiaomi 14 (HyperOS 3), used when no backup of the user's own exists. */
    private const val STOCK = """{
  "action": "android.media.action.STILL_IMAGE_CAMERA",
  "iconId": "@2131232903",
  "isOverlay": false,
  "isSystemDefault": false,
  "nameId": "@2132019651",
  "packageName": "com.android.camera",
  "tag": "CAMERA_CAPTURE",
  "targetClass": "com.android.camera.Camera",
  "targetPackage": "com.android.camera"
}"""

    fun grantCommand(ctx: Context) = "adb shell pm grant ${ctx.packageName} android.permission.WRITE_SECURE_SETTINGS"

    /** HyperOS / MIUI lock screen with shortcuts. */
    fun available(ctx: Context) = current(ctx) != null

    fun canWrite(ctx: Context) = ctx.checkSelfPermission(Manifest.permission.WRITE_SECURE_SETTINGS) == PackageManager.PERMISSION_GRANTED

    fun isOurs(ctx: Context) = current(ctx)?.let { runCatching { JSONObject(it).optString("targetPackage") }.getOrNull() } == ctx.packageName

    private fun current(ctx: Context): String? = Settings.Secure.getString(ctx.contentResolver, KEY)

    private fun ours(ctx: Context): String = JSONObject().apply {
        put("action", "android.media.action.STILL_IMAGE_CAMERA")
        put("iconId", "@${R.drawable.ic_tile}")
        put("isOverlay", false)
        put("isSystemDefault", false)
        put("nameId", "@${R.string.lock_shortcut_name}")
        put("packageName", ctx.packageName)
        put("tag", "CAMERA_CAPTURE")
        put("targetClass", CaptureActivity::class.java.name)
        put("targetPackage", ctx.packageName)
    }.toString(2)

    /** Puts RawZero on the lock screen camera button; the previous value is kept for [restore]. */
    fun install(ctx: Context) {
        val prev = current(ctx)
        if (prev != null && !isOurs(ctx)) ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putString("backup", prev).apply()
        Settings.Secure.putString(ctx.contentResolver, KEY, ours(ctx))
    }

    /** Gives the button back to the stock camera (or whatever was there before). */
    fun restore(ctx: Context) {
        val backup = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString("backup", null) ?: STOCK
        Settings.Secure.putString(ctx.contentResolver, KEY, backup)
    }

    /** Resource ids can move between builds: rewrite our entry if it names stale ones. */
    fun refresh(ctx: Context) {
        if (isOurs(ctx) && canWrite(ctx) && current(ctx) != ours(ctx)) install(ctx)
    }
}
