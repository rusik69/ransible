#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.php ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-php.git roles/geerlingguy.php
