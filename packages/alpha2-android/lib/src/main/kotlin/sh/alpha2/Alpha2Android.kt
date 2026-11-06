package sh.alpha2

import android.content.Context
import android.view.Surface

/**
 * What the generated API cannot express: the app context the library needs for audio output and
 * codec listing, and the native window behind a [Surface] for [Player.attachWindow].
 */
object Alpha2Android {
    init {
        System.loadLibrary("alpha2_mobile")
    }

    /** Call once, before anything else, with any context of the app. */
    @JvmStatic external fun init(context: Context)

    /**
     * The native window of [surface], as the handle [Player.attachWindow] takes over, or 0 when
     * the surface has none. Detach it before the surface is destroyed.
     */
    @JvmStatic external fun windowFromSurface(surface: Surface): Long
}
