#!/usr/bin/env python3
"""Limit concurrency and Asupersync compiler memory without changing sources or features."""
import os
import shutil
import sys

compiler, *arguments = sys.argv[1:]
if '--crate-name' in arguments and any(arg.startswith('--emit=') for arg in arguments):
    arguments += ['-Zunstable-options', '--jobs-backend=1', '--jobs-linker=1']
    if arguments[arguments.index('--crate-name') + 1] == 'asupersync':
        arguments += ['-Zfewer-names=yes', '-Zmir-opt-level=0',
            '-Zhint-mostly-unused=yes', '-Ztime-passes']
resolved = compiler if '/' in compiler else shutil.which(compiler)
if resolved is None:
    raise SystemExit('Compiler executable is unavailable on the configured PATH')
os.execv(resolved, [compiler, *arguments])
