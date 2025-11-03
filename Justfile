default:
    @just --list

compose name="":
    docker compose build {{name}}
    docker compose down {{name}}
    docker compose up {{name}} -d

log name:
    docker logs routini-{{name}}-1
