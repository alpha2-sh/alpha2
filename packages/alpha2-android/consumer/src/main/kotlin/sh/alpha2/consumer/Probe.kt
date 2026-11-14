package sh.alpha2.consumer

import android.content.Context
import java.io.File
import kotlinx.coroutines.runBlocking
import sh.alpha2.Alpha2
import sh.alpha2.Alpha2Android
import sh.alpha2.EngineConfig
import sh.alpha2.ExportProgress
import sh.alpha2.ExportProgressEvent
import sh.alpha2.ExportRequest
import sh.alpha2.RendererMode
import sh.alpha2.VideoDecodeMode
import sh.alpha2.VideoFrameMode

/** App code that calls the library the way an app does. */
object Probe {
    private fun engine(context: Context): Alpha2 {
        Alpha2Android.init(context)
        return Alpha2(EngineConfig(RendererMode.AUTO, VideoDecodeMode.AUTO, VideoFrameMode.AUTO))
    }

    /** The capabilities JSON. */
    fun capabilities(context: Context): String = engine(context).use { it.capabilities() }

    /** Exports [projectJson] with [assets] to [output] and returns the report JSON. */
    fun export(context: Context, projectJson: String, assets: Map<String, String>, output: File): String =
        engine(context).use { engine ->
            val request = ExportRequest(
                projectJson = projectJson,
                assets = assets,
                frames = emptyMap(),
                resources = emptyMap(),
                outputPath = output.absolutePath,
                overwrite = true,
                container = null,
                videoCodec = null,
                audioCodec = null,
                includeAudio = true,
                startTicks = null,
                endTicks = null,
            )
            val progress = object : ExportProgress {
                override fun onProgress(event: ExportProgressEvent) {}
            }
            runBlocking { engine.export(request, progress).use { it.outcome() } }.reportJson
        }
}
