package main

import (
	"fmt"
	"io"
	"log"
	"net/http"
	"path/filepath"
	"strings"

	"github.com/gin-gonic/gin"
	"github.com/otiai10/gosseract/v2"
	"golang.org/x/net/html"
)

type OCRLine struct {
	Text string `json:"text"`
	BBox []int  `json:"bbox"` // [x1 y1 x2 y2]
	Conf int    `json:"conf,omitempty"`
}

func attr(n *html.Node, key string) (string, bool) {
	for _, a := range n.Attr {
		if a.Key == key {
			return a.Val, true
		}
	}
	return "", false
}

// walk the tree, collect every <span class="ocr_line">
func collectLines(n *html.Node, outLines *[]OCRLine) {

	if n.Type == html.ElementNode && n.Data == "span" {
		// check class attribute
		if class, ok := attr(n, "class"); ok && class == "ocr_line" {
			// get inner text
			var txt string
			var f func(*html.Node)
			f = func(c *html.Node) {
				if c.Type == html.TextNode {
					txt += c.Data
				}
				for ch := c.FirstChild; ch != nil; ch = ch.NextSibling {
					f(ch)
				}
			}
			f(n)

			// parse title: “bbox 95 14 211 31; x_wconf 90”
			title, _ := attr(n, "title")
			var x1, y1, x2, y2, conf int
			fmt.Sscanf(title,
				"bbox %d %d %d %d; x_wconf %d",
				&x1, &y1, &x2, &y2, &conf)

			*outLines = append(*outLines, OCRLine{
				Text: txt,
				BBox: []int{x1, y1, x2, y2},
				Conf: conf,
			})
		}
	}
	// recurse
	for c := n.FirstChild; c != nil; c = c.NextSibling {
		collectLines(c, outLines)
	}
}

func main() {
	client := gosseract.NewClient()
	defer client.Close()
	log.Println("client started")

	// client.Languages = append(client.Languages, "deu")
	// log.Println("Detecting Languages", client.Languages)

	// http server
	router := gin.Default()
	router.POST("/", func(c *gin.Context) {

		f, err := c.FormFile("file")
		if err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": "no file"})
		}

		selectedLanguage := c.Query("language")
		// TODO add more languages
		if selectedLanguage == "eng" || selectedLanguage == "deu" {
			client.SetLanguage(selectedLanguage)
		}
		log.Println(client.Languages)

		// check extension
		// TODO filter by file type
		extension := filepath.Ext(f.Filename)
		if extension != ".png" && extension != ".jpg" { // && extension != ".pdf" && extension != ".docx" {
			c.JSON(http.StatusBadRequest, gin.H{"error": fmt.Sprintf("unsupported type: %s", extension)})
			return
		}

		src, err := f.Open()
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": "unable to open file"})
		}
		defer src.Close()

		byteData, err := io.ReadAll(src)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": "unable to read data from file"})
		}

		client.SetImageFromBytes(byteData)
		text, err := client.HOCRText()
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": "unable to extract text via tesseract"})
		}

		// parse hOCR format to list of OCR Elements
		doc, err := html.Parse(strings.NewReader(text))
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": "unable to parse OCR elements"})
		}
		log.Println(doc)

		var lines []OCRLine
		collectLines(doc, &lines)

		c.JSON(http.StatusOK, gin.H{"text": text, "elements": lines, "language": selectedLanguage})

		// reset language
		client.SetLanguage("eng")
	})

	router.Run("0.0.0.0:8080")
}
