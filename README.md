# dgg-phrases

Collects banned phrases from dgg and puts them into an Postgres DB.

---

## How to deploy

1. ```cp .env.example .env```
2. Edit the ```.env``` file.
3. ```cp banned_memes.txt.example banned_memes.txt```
4. ```nano banned_memes.txt``` (add phrases that you don't want added to the db)
5. ```cargo build --release```
6. ```dgg-phrases```

or, if you wanna use Docker

4. ```docker build -t dgg-phrases .```
5. ```docker run --env-file .env -it dgg-phrases```