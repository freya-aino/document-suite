FROM golang:alpine3.23

RUN apk update

# install deps
WORKDIR /tmp/app
COPY ./server/go.mod ./server/go.sum ./
RUN go mod download

# # build project
# RUN go build -o /app/main ./main.go

# # install sqlc
# RUN go install github.com/sqlc-dev/sqlc/cmd/sqlc@latest
# TODO - maybe need to add go dir to path for cli access
# RUN sqlc generate

# install migrate
RUN curl -L https://github.com/golang-migrate/migrate/releases/download/$version/migrate.$os-$arch.tar.gz | tar xvz
# RUN migrate -path ./migrations up -database

COPY ./migrations .
