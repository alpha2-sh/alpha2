package sh.alpha2

import android.content.Context
import android.view.Surface

/**
 * What the generated API cannot express: the app context the library needs for audio output and
 * codec listing, and the native window behind a [Surface] for a [Player].
 */
object Alpha2Android {
    init {
        System.loadLibrary("alpha2_mobile")
    }

    /** Call once, before anything else, with any context of the app. */
    @JvmStatic external fun init(context: Context)

    /**
     * Draws [player]'s preview into [surface], of [width] x [height] pixels, replacing any
     * attached window. The player holds its own reference to the surface's window; call
     * [Player.detachWindow] before the surface is destroyed.
     *
     * @throws IllegalArgumentException when the surface has no native window.
     */
    @JvmStatic
    fun attach(player: Player, surface: Surface, width: UInt, height: UInt) {
        val window = windowFromSurface(surface)
        require(window != 0L) { "the surface has no native window" }
        player.attachWindow(window.toULong(), width, height)
    }

    /** The native window of [surface] with a reference of its own, or 0 when it has none. */
    @JvmStatic private external fun windowFromSurface(surface: Surface): Long
}
