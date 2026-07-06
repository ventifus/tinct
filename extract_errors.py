#!/usr/bin/env python3
import json, sys, re

with open(sys.argv[1]) as f:
    data = json.load(f)

result = data.get('result', '')
# result is a JSON string itself
inner = json.loads(result)
stderr = inner.get('stderr', '')
stdout = inner.get('stdout', '')

text = stderr + stdout

# Extract error lines
for line in text.split('\\n'):
    if 'error[' in line or 'error:' in line or '-->' in line:
        print(line)
