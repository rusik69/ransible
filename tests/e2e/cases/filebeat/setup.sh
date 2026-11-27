#!/usr/bin/env bash
mkdir -p roles
[ -d roles/geerlingguy.filebeat ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-filebeat.git roles/geerlingguy.filebeat
