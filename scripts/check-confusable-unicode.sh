#!/bin/bash
# Check for confusable Unicode characters that look like ASCII equivalents.
#
# Detects:
#   U+2010 HYPHEN, U+2011 NB HYPHEN, U+2012 FIGURE DASH — look like -
#   U+2212 MINUS SIGN                                      — looks like -
#   U+00A0 NO-BREAK SPACE, U+202F NB NARROW SPACE          — look like space
#   U+200B ZERO WIDTH SPACE                                — invisible
#   U+FEFF BOM                                              — invisible

! rg -l \
    -e $'\xe2\x80\x90' \
    -e $'\xe2\x80\x91' \
    -e $'\xe2\x80\x92' \
    -e $'\xe2\x88\x92' \
    -e $'\xc2\xa0' \
    -e $'\xe2\x80\xaf' \
    -e $'\xe2\x80\x8b' \
    -e $'\xef\xbb\xbf' \
    "$@"
