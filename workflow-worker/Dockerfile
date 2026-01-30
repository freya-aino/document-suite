FROM golang:latest
# TODO - change latest to fixed

# Update
RUN apt-get update -y

# install deps
WORKDIR /tmp/app
COPY go.mod go.sum ./
RUN go mod download

# build project
COPY . .
# ENV GOOS=linux
# ENV GOARCH=amd64
RUN go build -o /app/main .

# serve
WORKDIR /app
COPY config.toml /app/
CMD [ "./main" ]