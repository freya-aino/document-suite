FROM golang:alpine3.23

# Update
RUN apk update

ENV CGO_ENABLED=1

# install tesseract and language packs
RUN apk add --no-cache build-base
RUN apk add --no-cache tesseract-ocr tesseract-ocr-dev leptonica-dev
RUN apk add --no-cache tesseract-ocr-data-deu tesseract-ocr-data-eng


# install deps
WORKDIR /tmp/app
COPY ./go.mod ./go.sum ./
RUN go mod download

# build project
COPY . .
RUN go build -o /app/main .

# serve
WORKDIR /app
ENTRYPOINT ["./main"]
