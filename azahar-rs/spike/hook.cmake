# Pulled into Azahar's own configure step through -DCMAKE_PROJECT_citra_INCLUDE=<this file>.
# Azahar's CMake files hardcode CMAKE_SOURCE_DIR for Boost and zstd, so the tree cannot be a
# subdirectory of a wrapper project; instead spike.cmake is included into Azahar's top-level
# directory once every Azahar target exists, leaving the third-party tree untouched.
# (add_subdirectory is not allowed in deferred execution, and the arguments of a deferred call
# are expanded when it runs, so the path is captured in a variable now.)
set(SUPERSHUCKIE_SPIKE_CMAKE "${CMAKE_CURRENT_LIST_DIR}/spike.cmake")
cmake_language(DEFER DIRECTORY "${CMAKE_SOURCE_DIR}" CALL include "${SUPERSHUCKIE_SPIKE_CMAKE}")
