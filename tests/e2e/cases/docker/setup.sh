#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.docker ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-docker.git roles/geerlingguy.docker
