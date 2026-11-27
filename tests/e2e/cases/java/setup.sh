#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.java ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-java.git roles/geerlingguy.java
