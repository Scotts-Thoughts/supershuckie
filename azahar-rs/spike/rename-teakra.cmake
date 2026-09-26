# Run with `cmake -DBUNDLE=<libazahar.a> -DTEAKRA=<azahar's libteakra.a> -DNM=<nm> -DOBJCOPY=<objcopy> -P rename-teakra.cmake`.
#
# melonDS (the DS core) and Azahar both ship Teakra, the DSP emulator, in different versions
# with the same symbol names. A binary linking both cores would see duplicate definitions, so
# every symbol Azahar's Teakra defines is renamed throughout the bundle (definitions and the
# references audio_core makes to them alike), which keeps the two copies apart.
execute_process(COMMAND "${NM}" -g --defined-only "${TEAKRA}" OUTPUT_VARIABLE symbols RESULT_VARIABLE result)
if(NOT result EQUAL 0)
    message(FATAL_ERROR "nm failed on ${TEAKRA}")
endif()
string(REPLACE "\n" ";" lines "${symbols}")
set(renames "")
set(seen "")
foreach(line IN LISTS lines)
    # "<address> <type> <name>"; skip archive member headers and blank lines.
    if(line MATCHES "^[0-9a-fA-F]+ [TDBRWV] ([^ ]+)$")
        set(name "${CMAKE_MATCH_1}")
        list(FIND seen "${name}" index)
        if(index EQUAL -1)
            list(APPEND seen "${name}")
            string(APPEND renames "${name} azahar_${name}\n")
        endif()
    endif()
endforeach()
list(LENGTH seen count)
if(count EQUAL 0)
    message(FATAL_ERROR "no Teakra symbols found in ${TEAKRA}")
endif()
file(WRITE "${BUNDLE}.teakra-renames.txt" "${renames}")
execute_process(COMMAND "${OBJCOPY}" "--redefine-syms=${BUNDLE}.teakra-renames.txt" "${BUNDLE}" RESULT_VARIABLE result)
if(NOT result EQUAL 0)
    message(FATAL_ERROR "objcopy failed on ${BUNDLE}")
endif()
message(STATUS "renamed ${count} Teakra symbols in ${BUNDLE}")
