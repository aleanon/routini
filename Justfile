default:
    @just --list

compose name="":
    docker compose build {{name}}
    docker compose down {{name}}
    docker compose up {{name}} -d

log name:
    docker logs routini-{{name}}-1

# Reload the config in the running container without restarting
reload:
    docker compose kill -s SIGHUP routini
