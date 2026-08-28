#!/bin/bash

rustc getwine.rs \
  -C opt-level=3 \
  -C lto=fat \
  -C codegen-units=1 \
  -C panic=abort \
  -C strip=symbols

