# Included by whisper.cpp's and ggml's project() calls: whisper-rs-sys passes CMAKE_* variables on.

# cmake-rs replaces CMake's release flags for MSVC and leaves out the optimization and NDEBUG.
function(alpha2_restore_release_flags)
  foreach(lang C CXX)
    if(NOT "${CMAKE_${lang}_FLAGS_RELEASE}" MATCHES "[-/]O[12x]")
      set(CMAKE_${lang}_FLAGS_RELEASE "${CMAKE_${lang}_FLAGS_RELEASE} /O2 /Ob2 /DNDEBUG" PARENT_SCOPE)
    endif()
  endforeach()
endfunction()

function(alpha2_replace_in_whisper what from to)
  set(source "${PROJECT_SOURCE_DIR}/src/whisper.cpp")
  file(READ "${source}" text)
  string(FIND "${text}" "${to}" done)
  if(NOT done EQUAL -1)
    return()
  endif()
  string(FIND "${text}" "${from}" at)
  if(at EQUAL -1)
    message(FATAL_ERROR "whisper.cpp no longer has the ${what} that transcribe's cmake/whisper.cmake replaces; remove that replacement")
  endif()
  string(REPLACE "${from}" "${to}" text "${text}")
  file(WRITE "${source}" "${text}")
endfunction()

if(MSVC)
  alpha2_restore_release_flags()
endif()
if(PROJECT_NAME STREQUAL "whisper.cpp")
  # whisper.cpp 1.8.3 asserts that its DTW median filter is narrower than the decoded stretch;
  # its master branch narrows the filter instead.
  alpha2_replace_in_whisper("DTW median filter assert"
    "    WHISPER_ASSERT(filter_width < a->ne[2]);\n"
    "    if (filter_width >= a->ne[2]) {\n        filter_width = (a->ne[2] % 2) ? (int) a->ne[2] - 2 : (int) a->ne[2] - 1;\n        if (filter_width < 1) {\n            filter_width = 1;\n        }\n    }\n    WHISPER_ASSERT(filter_width < a->ne[2] || filter_width == 1);\n")
  # whisper.cpp deletes a context whose model failed to load without freeing the model's buffers,
  # and ggml's Metal device asserts at exit that every buffer was freed.
  alpha2_replace_in_whisper("model load failure's delete"
    "        WHISPER_LOG_ERROR(\"%s: failed to load model\\n\", __func__);\n        delete ctx;\n"
    "        WHISPER_LOG_ERROR(\"%s: failed to load model\\n\", __func__);\n        whisper_free(ctx);\n")
endif()
