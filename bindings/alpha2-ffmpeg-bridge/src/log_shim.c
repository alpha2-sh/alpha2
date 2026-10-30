/* Captures FFmpeg's warning and error lines for typed errors, and still prints every line the way
 * FFmpeg's default callback does. */
#include <stdarg.h>
#include <stddef.h>
#include "alpha2_ffmpeg_version.h"
#include <libavutil/log.h>

/* Defined in log.rs: keeps one formatted line with the name of the context that logged it. */
extern void alpha2_log_record(int level, const char *class_name, const char *line);

static void alpha2_log_callback(void *ptr, int level, const char *fmt, va_list vl) {
    if (level <= AV_LOG_WARNING) {
        char line[1024];
        int print_prefix = 0;
        va_list copy;
        va_copy(copy, vl);
        av_log_format_line2(ptr, level, fmt, copy, line, (int)sizeof line, &print_prefix);
        va_end(copy);
        const char *class_name = "";
        if (ptr != NULL) {
            const AVClass *class = *(const AVClass *const *)ptr;
            if (class != NULL && class->item_name != NULL) class_name = class->item_name(ptr);
        }
        alpha2_log_record(level, class_name != NULL ? class_name : "", line);
    }
    av_log_default_callback(ptr, level, fmt, vl);
}

void alpha2_log_install(void) { av_log_set_callback(alpha2_log_callback); }
