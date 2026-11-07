package dev.vegameta.rawzero

import android.app.PendingIntent
import android.content.Intent
import android.os.Build
import android.provider.MediaStore
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService

/**
 * Quick Settings / Control Center tile that opens the camera, also from the lock screen
 * (vendor lock screen shortcuts such as HyperOS' accept only system apps). On a locked
 * phone it starts the secure camera intent: CaptureActivity shows over the keyguard and
 * keeps the gallery behind an unlock.
 */
class CameraTileService : TileService() {

    override fun onStartListening() {
        qsTile?.apply { state = Tile.STATE_INACTIVE; updateTile() }
    }

    override fun onClick() {
        val intent = Intent(this, CaptureActivity::class.java)
            .setAction(if (isLocked) MediaStore.INTENT_ACTION_STILL_IMAGE_CAMERA_SECURE else MediaStore.INTENT_ACTION_STILL_IMAGE_CAMERA)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        if (Build.VERSION.SDK_INT >= 34) {
            startActivityAndCollapse(PendingIntent.getActivity(this, 0, intent, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT))
        } else {
            @Suppress("DEPRECATION")
            startActivityAndCollapse(intent)
        }
    }
}
