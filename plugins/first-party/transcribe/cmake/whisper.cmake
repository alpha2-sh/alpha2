# Included by whisper.cpp's and ggml's project() calls: whisper-rs-sys passes CMAKE_* variables on.

# cmake-rs replaces CMake's release flags for MSVC and leaves out the optimization and NDEBUG.
function(alpha2_restore_release_flags)
  foreach(lang C CXX)
    if(NOT "${CMAKE_${lang}_FLAGS_RELEASE}" MATCHES "[-/]O[12x]")
      set(CMAKE_${lang}_FLAGS_RELEASE "${CMAKE_${lang}_FLAGS_RELEASE} /O2 /Ob2 /DNDEBUG" PARENT_SCOPE)
    endif()
  endforeach()
endfunction()

# whisper.cpp 1.8.3 asserts that its DTW median filter is narrower than the decoded stretch;
# its master branch narrows the filter instead.
function(alpha2_narrow_dtw_median_filter)
  set(source "${PROJECT_SOURCE_DIR}/src/whisper.cpp")
  file(READ "${source}" text)
  set(asserted "    WHISPER_ASSERT(filter_width < a->ne[2]);\n")
  set(narrowed "    if (filter_width >= a->ne[2]) {\n        filter_width = (a->ne[2] % 2) ? (int) a->ne[2] - 2 : (int) a->ne[2] - 1;\n        if (filter_width < 1) {\n            filter_width = 1;\n        }\n    }\n    WHISPER_ASSERT(filter_width < a->ne[2] || filter_width == 1);\n")
  string(FIND "${text}" "${narrowed}" done)
  if(NOT done EQUAL -1)
    return()
  endif()
  string(FIND "${text}" "${asserted}" at)
  if(at EQUAL -1)
    message(FATAL_ERROR "whisper.cpp no longer has the DTW median filter assert that transcribe's cmake/whisper.cmake replaces; remove alpha2_narrow_dtw_median_filter")
  endif()
  string(REPLACE "${asserted}" "${narrowed}" text "${text}")
  file(WRITE "${source}" "${text}")
endfunction()

if(MSVC)
  alpha2_restore_release_flags()
endif()
if(PROJECT_NAME STREQUAL "whisper.cpp")
  alpha2_narrow_dtw_median_filter()
endif()
