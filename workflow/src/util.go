package src

import (
	"bytes"
	"encoding/hex"
	"io"
	"mime/multipart"
	"os"
	"reflect"
	"runtime"
	"strings"

	"github.com/samborkent/uuidv7"
)

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
