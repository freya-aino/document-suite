package core

import (
	"bytes"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"mime/multipart"
	"os"
	"reflect"
	"runtime"
	"strings"

	"github.com/samborkent/uuidv7"
)

func PGConnectionString() (string, error) {
	pgUser := os.Getenv("POSTGRES_USER")
	pgPassword := os.Getenv("POSTGRES_PASSWORD")
	pgPort := os.Getenv("POSTGRES_PORT")
	pgAddress := os.Getenv("POSTGRES_ADDRESS")

	if pgUser == "" {
		return "", errors.New("pgUser not provided")
	}
	if pgPassword == "" {
		return "", errors.New("pgPassword not provided")
	}

	connectionString := fmt.Sprintf("postgres://%s:%s@%s:%s/", pgUser, pgPassword, pgAddress, pgPort)
	return connectionString, nil
}

func UUIDFromString(s string) (uuidv7.UUID, error) {
	// strip the hyphens
	clean := strings.ReplaceAll(s, "-", "")
	// decode the 32‑hex chars → 16 bytes
	b, err := hex.DecodeString(clean)
	if err != nil {
		return uuidv7.UUID{}, err
	}
	var id uuidv7.UUID // [16]byte
	copy(id[:], b)     // copy into the fixed‑size array
	return id, nil
}

func funcName(i interface{}) string {
	return runtime.FuncForPC(reflect.ValueOf(i).Pointer()).Name()
}

func GetFileData(file *multipart.FileHeader) ([]byte, error) {

	// open file
	fileSource, err := file.Open()
	if err != nil {
		return nil, err
	}
	defer fileSource.Close()

	fielData, err := io.ReadAll(fileSource)
	if err != nil {
		return nil, err
	}

	return fielData, nil
}

func WriteAsTemp(data *[]byte) (string, error) {
	tmp, err := os.CreateTemp("", "img-*")
	if err != nil {
		return "", err
	}

	reader := bytes.NewReader(*data)
	io.Copy(tmp, reader)
	tmp.Close()

	return tmp.Name(), nil
}
