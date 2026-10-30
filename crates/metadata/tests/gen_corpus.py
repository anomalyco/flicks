"""Writes tests/corpus.tsv: 2,000 library paths in the naming styles Plex, Jellyfin, scene releases,
YTS-style, fansubs and old rips use, each with the answer known by construction.

    python3 -I crates/metadata/tests/gen_corpus.py crates/metadata/tests

It also writes library.tsv (a 200-title library: path, the TMDB id it should match, kind) and tmdb.json (the TMDB v3 search responses for that library, see below). The output
is committed, so this only needs running to change the test data.

corpus.tsv columns: path, title, year, season, episode, episode_end, edition (empty = none). Titles
are the real ones; the test compares them with punctuation and case ignored.

tmdb.json maps "movie|<normalized title>|<year or empty>" (or "tv|...") to the body TMDB's
/search/movie or /search/tv answers with: its documented v3 shape, with the true entry, same-title
remakes ranked first in the unfiltered search for one title in ten (TMDB sorts by popularity). The ids are made up; there was no API key to record real ones with.
"""
import json
import random
import re
import sys

MOVIES = """The Shawshank Redemption|1994;The Godfather|1972;The Dark Knight|2008;Pulp Fiction|1994;Forrest Gump|1994;Inception|2010;Fight Club|1999;The Matrix|1999;Goodfellas|1990;Se7en|1995;Interstellar|2014;Parasite|2019;Gladiator|2000;Whiplash|2014;The Prestige|2006;Alien|1979;Aliens|1986;Blade Runner|1982;Blade Runner 2049|2017;Heat|1995;Up|2009;Her|2013;It|2017;Us|2019;Dune|2021;Dune: Part Two|2024;1917|2019;2012|2009;300|2006;21 Jump Street|2012;Apollo 13|1995;District 9|2009;Wonder Woman 1984|2020;Spider-Man: No Way Home|2021;Mission: Impossible - Fallout|2018;Ocean's Eleven|2001;E.T. the Extra-Terrestrial|1982;S.W.A.T.|2003;The Lord of the Rings: The Fellowship of the Ring|2001;Star Wars: Episode IV - A New Hope|1977;Back to the Future|1985;Back to the Future Part II|1989;Mad Max: Fury Road|2015;Everything Everywhere All at Once|2022;Oppenheimer|2023;Barbie|2023;Top Gun: Maverick|2022;Avatar|2009;Titanic|1997;Jurassic Park|1993;The Lion King|1994;Toy Story 3|2010;Finding Nemo|2003;Coco|2017;Soul|2020;Tenet|2020;Joker|2019;Logan|2017;Deadpool & Wolverine|2024;Fast & Furious 6|2013;Mr. & Mrs. Smith|2005;Dr. Strangelove|1964;Monty Python and the Holy Grail|1975;Amélie|2001;Léon: The Professional|1994;Pan's Labyrinth|2006;Crouching Tiger, Hidden Dragon|2000;Spirited Away|2001;Princess Mononoke|1997;Seven Samurai|1954;Casablanca|1942;Citizen Kane|1941;Psycho|1960;Vertigo|1958;Rear Window|1954;Taxi Driver|1976;Raging Bull|1980;Apocalypse Now|1979;Full Metal Jacket|1987;Platoon|1986;Saving Private Ryan|1998;Schindler's List|1993;The Social Network|2010;Moneyball|2011;Arrival|2016;Sicario|2015;Prisoners|2013;Gone Girl|2014;Zodiac|2007;No Country for Old Men|2007;There Will Be Blood|2007;The Departed|2006;Black Swan|2010;La La Land|2016;Get Out|2017;A Quiet Place|2018;Hereditary|2018;Midsommar|2019;The Witch|2015;Ex Machina|2014;Moon|2009;Primer|2004;Memento|2000;Cube|1997;Tron: Legacy|2010;Ready Player One|2018;The Grand Budapest Hotel|2014;Knives Out|2019;Glass Onion|2022;Nope|2022;Babylon|2022;Tár|2022;The Banshees of Inisherin|2022;CODA|2021;Ford v Ferrari|2019;Mank|2020;Roma|2018;Bohemian Rhapsody|2018;Hot Fuzz|2007;Shaun of the Dead|2004;Superbad|2007;Bridesmaids|2011;Groundhog Day|1993;Big|1988;Elf|2003;Home Alone|1990;Die Hard|1988;Lethal Weapon|1987;Speed|1994;Face/Off|1997;Ghostbusters|1984;Gremlins|1984;The Thing|1982;Predator|1987;RoboCop|1987;Terminator 2: Judgment Day|1991;The Terminator|1984;Raiders of the Lost Ark|1981;Jaws|1975;Rocky|1976;Creed|2015;Hidden Figures|2016;Whale Rider|2002;The Hurt Locker|2008;Argo|2012;Spotlight|2015;Birdman|2014;Amadeus|1984;Scarface|1983;Blow|2001;Drive|2011;Bullitt|1968;Sunshine|2007;Wall-E|2008;Ratatouille|2007;Inside Out|2015;Encanto|2021;Frozen II|2019;Shrek 2|2004;Kung Fu Panda|2008;Up in the Air|2009;Your Name|2016;Akira|1988;Perfect Blue|1997;Ran|1985;Oldboy|2003;Train to Busan|2016;The Raid|2011;Rush|2013;Se7en|1995""".split(";")
MOVIES = [(t, int(y)) for t, y in (m.split("|") for m in MOVIES)]
# Titles with a year inside them have no unambiguous yearless form ("Wonder Woman 1984.mkv").
YEAR_IN_TITLE = {"Wonder Woman 1984", "1917", "2012"}

SHOWS = """Breaking Bad|2008;Better Call Saul|2015;The Wire|2002;The Sopranos|1999;Game of Thrones|2011;House of the Dragon|2022;Stranger Things|2016;Peaky Blinders|2013;Black Mirror|2011;Chernobyl|2019;Succession|2018;Severance|2022;Andor|2022;The Mandalorian|2019;The Office (US)|2005;Parks and Recreation|2009;Brooklyn Nine-Nine|2013;Mr. Robot|2015;The 100|2014;24|2001;9-1-1|2018;Lost|2004;Fringe|2008;Dark|2017;Doctor Who|2005;Sherlock|2010;Star Trek: The Next Generation|1987;Star Trek: Deep Space Nine|1993;Marvel's Agents of S.H.I.E.L.D.|2013;Law & Order: Special Victims Unit|1999;Friends|1994;Seinfeld|1989;Frasier|1993;Cheers|1982;The Simpsons|1989;South Park|1997;Rick and Morty|2013;Bojack Horseman|2014;Archer|2009;Community|2009;Arrested Development|2003;It's Always Sunny in Philadelphia|2005;What We Do in the Shadows|2019;Ted Lasso|2020;Barry|2018;Fargo|2014;True Detective|2014;Westworld|2016;The Expanse|2015;Battlestar Galactica|2004;Firefly|2002;Dexter: New Blood|2021;Mad Men|2007;Boardwalk Empire|2010;Band of Brothers|2001;The Crown|2016;Downton Abbey|2010;Line of Duty|2012;Slow Horses|2022;Shogun|2024;1883|2021;1923|2022;Yellowjackets|2021;The Bear|2022;Only Murders in the Building|2021;Reacher|2022;Silo|2023;Foundation|2021;Love, Death & Robots|2019;Mythbusters|2003;Planet Earth II|2016;Cosmos: A Spacetime Odyssey|2014;Death Note|2006;Cowboy Bebop|1998;One Piece|1999;Attack on Titan|2013;Fullmetal Alchemist: Brotherhood|2009;Steins@Gate|2011""".split(";")
SHOWS = [(t.replace("@", ";"), int(y)) for t, y in (m.split("|") for m in SHOWS)]
ANIME = {"Death Note", "Cowboy Bebop", "One Piece", "Attack on Titan", "Fullmetal Alchemist: Brotherhood", "Steins;Gate"}

EPISODE_TITLES = ["Pilot", "Cat's in the Bag", "The One Where They Meet", "Ozymandias", "Winter Is Coming", "Part 1", "Chapter 3", "Hello, World", "Gimme Shelter", "The Long Night", "Finale", "Who Am I", "Day One"]
GROUPS = ["SubsPlease", "Erai-raws", "HorribleSubs", "Judas", "ASW", "Commie", "Anime Time", "DB"]
SCENE = ["GROUP", "FGT", "RARBG", "YIFY", "NTb", "FLUX", "CMRG", "SPARKS", "TEPES", "EVO", "playWEB"]
EDITIONS = [("Director's Cut", "Director's Cut"), ("Extended Cut", "Extended"), ("Extended Edition", "Extended"), ("Theatrical Cut", "Theatrical"), ("Unrated", "Unrated"), ("Special Edition", "Special Edition"), ("Remastered", "Remastered"), ("IMAX", "IMAX"), ("Final Cut", "Final Cut"), ("Ultimate Edition", "Ultimate Edition")]
EXT = ["mkv", "mkv", "mkv", "mp4", "avi", "m4v", "ts"]

rng = random.Random(2026)


def fs(t):
    """Name as written on a case-insensitive filesystem: no colons or slashes."""
    return t.replace(": ", " - ").replace(":", "").replace("/", "-").replace("?", "")


def dots(t, sep="."):
    t = t.replace(" - ", " ").replace(":", "").replace("'", "").replace(",", "").replace("&", "and").replace("/", " ").replace(" ", sep)
    return re.sub("\\" + sep + "+", sep, t)


def pick(xs):
    return xs[rng.randrange(len(xs))]


def release():
    res = pick(["720p", "1080p", "1080p", "2160p"])
    src = pick(["BluRay", "WEB-DL", "WEBRip", "HDTV", "BDRip", "BluRay.REMUX", "AMZN.WEB-DL", "DVDRip", "HDRip"])
    codec = pick(["x264", "x265", "H.264", "HEVC", "H.265", "XviD"])
    audio = pick(["", "", ".DDP5.1", ".DTS-HD.MA.5.1", ".AAC", ".AC3", ".Atmos", ".TrueHD.7.1"])
    hdr = pick(["", "", "", ".HDR", ".10bit", ".HDR10", ".DV"])
    return f"{res}.{src}{hdr}{audio}.{codec}-{pick(SCENE)}"


def movie_rows(n):
    rows = []
    for i in range(n):
        t, y = MOVIES[i % len(MOVIES)]
        ext = pick(EXT)
        ed = None
        yearless_ok = t not in YEAR_IN_TITLE
        style = rng.randrange(20)
        T, D = fs(t), dots(t)
        if style == 0:
            p = f"Movies/{T} ({y})/{T} ({y}).{ext}"
        elif style == 1:
            p = f"Movies/{T} ({y})/{T} ({y}) - {pick(['1080p', '720p', '2160p', '4K'])}.{ext}"
        elif style == 2:
            p = f"{D}.{y}.{release()}.{ext}"
        elif style == 3:
            rel = release()
            p = f"{D}.{y}.{rel}/{D}.{y}.{rel}.{ext}"
        elif style == 4:
            p = f"{T} ({y}) [{pick(['1080p', '720p'])}] [{pick(['BluRay', 'WEBRip'])}] [YTS.MX]/{T}.({y}).{pick(['1080p', '720p'])}.{pick(['BluRay', 'WEBRip'])}.x264-[YTS.MX].{ext}"
        elif style == 5:
            p = f"{dots(t, '_')}_{y}_{pick(['DVDRip', 'DVDSCR', 'BRRip'])}_{pick(['XviD', 'x264'])}-{pick(SCENE)}.{ext}"
        elif style == 6:
            p = f"{T} [{y}].{ext}"
        elif style == 7:
            p = f"Films/{T} ({y})/{T}.{ext}"
        elif style == 8:
            ed, _ = pick(EDITIONS)  # Plex edition tags keep the label as written
            p = f"Movies/{T} ({y})/{T} ({y}) {{edition-{ed}}}.{ext}"
        elif style == 9:
            e, label = pick(EDITIONS)
            p = f"{D}.{y}.{e.replace(' ', '.')}.{release()}.{ext}"
            ed = label
        elif style == 10:
            p = f"D:\\Movies\\{T} ({y})\\{T} ({y}).{ext}"
        elif style == 11:
            p = f"{D}.{y}.REPACK.1080p.AMZN.WEB-DL.DDP5.1.H.264-{pick(SCENE)}.{ext}"
        elif style == 12:
            e, label = pick([x for x in EDITIONS if x[0] not in ("IMAX", "Remastered", "Final Cut", "Ultimate Edition")])
            p = f"Movies/{T} ({y})/{T} ({y}) - {e}.{ext}"
            ed = label
        elif style == 13:
            p = f"Movies/{T} ({y}) [imdbid-tt{rng.randrange(1000000, 9999999)}]/{T} ({y}) [imdbid-tt{rng.randrange(1000000, 9999999)}].{ext}"
        elif style == 14:
            p = f"Movies/{T} ({y}) [tmdbid-{rng.randrange(10, 999999)}]/{T} ({y}) - {pick(['1080p', '2160p', '720p'])}.{ext}"
        elif style == 15:
            p = f"{T} ({y}) {pick(['4K', 'UHD', '1080p', '720p'])} {pick(['HDR', 'BluRay', 'WEB-DL', 'x265', 'HEVC'])}.{ext}"
        elif style == 16:
            p = f"/mnt/media/movies/{T} ({y})/{T} ({y}) CD{rng.randrange(1, 3)}.{ext}"
        elif style == 17 and yearless_ok:
            y = None
            p = f"Movies/{T}.{ext}"
        elif style == 18 and yearless_ok:
            y = None
            p = f"{D}.{release()}.{ext}"
        else:
            p = f"movies/{D.lower()}.{y}.{pick(['1080p', '720p'])}.{pick(['bluray', 'webrip', 'hdrip'])}.{pick(['x264', 'x265'])}-{pick(SCENE).lower()}.{ext}"
        rows.append((p, t, y, None, None, None, ed))
    return rows


def show_rows(n):
    rows = []
    for i in range(n):
        t, y = SHOWS[i % len(SHOWS)]
        anime = t in ANIME
        S = rng.randrange(1, 13)
        E = rng.randrange(1, 25)
        ep = pick(EPISODE_TITLES)
        ext = pick(EXT)
        T, D = fs(t), dots(t)
        s2, e2 = f"{S:02}", f"{E:02}"
        style = rng.randrange(18 if not anime else 22)
        yr, s, e, end = None, S, E, None
        if style == 0:
            yr = y
            p = f"TV/{T} ({y})/Season {s2}/{T} - S{s2}E{e2} - {fs(ep)}.{ext}"
        elif style == 1:
            p = f"TV/{T}/Season {S}/{T} - s{s2}e{e2}.{ext}"
        elif style == 2:
            p = f"{D}.S{s2}E{e2}.{pick(['720p', '1080p'])}.HDTV.x264-{pick(SCENE)}.{ext}"
        elif style == 3:
            p = f"{D}.S{s2}E{e2}.{dots(ep)}.1080p.WEB-DL.DDP5.1.H.264-{pick(SCENE)}.{ext}"
        elif style == 4:
            p = f"{T} {S}x{e2}.{ext}"
        elif style == 5:
            p = f"{T} - {S}x{e2} - {fs(ep)}.{ext}"
        elif style == 6:
            p = f"{T}/Season {S}/{e2} - {fs(ep)}.{ext}"
        elif style == 7:
            end = min(E + 1, 99)
            p = f"{T}/{T} S{s2}E{e2}-E{end:02}.{ext}"
        elif style == 8:
            yr = y
            p = f"{T} ({y}) - S{s2}E{e2} - {fs(ep)}.{ext}"
        elif style == 9:
            end = E + 1
            p = f"{T}/Season {s2}/{T} - S{s2}E{e2}E{end:02} - {fs(ep)}.{ext}"
        elif style == 10:
            p = f"{D}.Season.{S}.Episode.{E}.{ext}"
        elif style == 11:
            p = f"{T}/Season {S}/Episode {E}.{ext}"
        elif style == 12:
            p = f"D:\\TV\\{T}\\Season {s2}\\{T} - S{s2}E{e2}.{ext}"
        elif style == 13:
            s, e = 0, E
            p = f"{T}/Specials/{T} - S00E{e2} - {fs(ep)}.{ext}"
        elif style == 14:
            p = f"{T}/s{s2}/{D}.s{s2}e{e2}.{ext}"
        elif style == 15:
            yr = y
            p = f"Shows/{T} ({y}) [tvdbid-{rng.randrange(1000, 999999)}]/Season {s2}/{T} ({y}) - S{s2}E{e2} - {fs(ep)}.{ext}"
        elif style == 16:
            p = f"{D}.S{s2}E{e2}.{release()}.{ext}"
        elif style == 17:
            p = f"TV/{T}/Season {s2}/{T} - S{s2}E{e2}.{ext}"
        elif style in (18, 19):
            g = pick(GROUPS)
            s = None
            e = E + rng.randrange(0, 100)
            p = f"[{g}] {T} - {e:02} [{pick(['1080p', '720p'])}][{rng.randrange(0x10000000, 0xFFFFFFFF):08X}].{ext}"
        elif style == 20:
            g = pick(GROUPS)
            s = None
            e = E
            p = f"[{g}] {T} - {e:02}v2 [BD 1080p HEVC FLAC].{ext}"
        else:
            g = pick(GROUPS)
            p = f"[{g}] {T} - S{s2}E{e2} [{pick(['1080p', '720p'])}].{ext}"
        # "(US)" only tells two shows with one title apart; the title is "The Office".
        rows.append((p, t.replace(" (US)", ""), yr, s, e, end, None))
    return rows


HAND = [
    ("Movies/Spider-Man (2002)/Spider-Man (2002).mkv", "Spider-Man", 2002, None, None, None, None),
    ("Movies/Monsters, Inc. (2001)/Monsters, Inc. (2001).mkv", "Monsters, Inc.", 2001, None, None, None, None),
    ("The.Matrix.Reloaded.2003.1080p.BluRay.x264-GRP.mkv", "The Matrix Reloaded", 2003, None, None, None, None),
    ("Blade.Runner.2049.2017.2160p.UHD.BluRay.REMUX.HDR.HEVC.Atmos-GRP.mkv", "Blade Runner 2049", 2017, None, None, None, None),
    ("Wonder Woman 1984 (2020)/Wonder Woman 1984 (2020).mkv", "Wonder Woman 1984", 2020, None, None, None, None),
    ("Wonder.Woman.1984.2020.1080p.WEB-DL.DDP5.1.Atmos.H.264-GRP.mkv", "Wonder Woman 1984", 2020, None, None, None, None),
    ("1917.2019.1080p.BluRay.x264-GRP.mkv", "1917", 2019, None, None, None, None),
    ("2001 A Space Odyssey (1968).mkv", "2001 A Space Odyssey", 1968, None, None, None, None),
    ("The.Lord.of.the.Rings.The.Return.of.the.King.2003.EXTENDED.1080p.BluRay.x264-GRP.mkv", "The Lord of the Rings The Return of the King", 2003, None, None, None, "Extended"),
    ("Apocalypse Now (1979) {edition-Final Cut}.mkv", "Apocalypse Now", 1979, None, None, None, "Final Cut"),
    ("Blade Runner (1982) {edition-The Final Cut}.mkv", "Blade Runner", 1982, None, None, None, "The Final Cut"),
    ("Kingdom of Heaven (2005) {edition-Director's Cut}.mkv", "Kingdom of Heaven", 2005, None, None, None, "Director's Cut"),
    ("Doctor.Who.2005.S01E01.Rose.720p.HDTV.x264-GRP.mkv", "Doctor Who", 2005, 1, 1, None, None),
    ("Mr.Robot.S02E03.1080p.WEB-DL.DD5.1.H264-GRP.mkv", "Mr Robot", None, 2, 3, None, None),
    ("The.100.S03E05.720p.HDTV.x264-GRP.mkv", "The 100", None, 3, 5, None, None),
    ("24.S08E24.720p.HDTV.x264-GRP.mkv", "24", None, 8, 24, None, None),
    ("9-1-1.S02E10.1080p.WEB.h264-GRP.mkv", "9-1-1", None, 2, 10, None, None),
    ("1883.S01E01.1080p.WEB.H264-GRP.mkv", "1883", None, 1, 1, None, None),
    ("Shows/The Office (US)/Season 3/The Office (US) - S03E12 - The Negotiation.mkv", "The Office", None, 3, 12, None, None),
    ("TV Shows/Friends/Season 10/Friends - S10E17 - The Last One.mkv", "Friends", None, 10, 17, None, None),
    ("anime/[SubsPlease] Frieren - 12 (1080p) [A1B2C3D4].mkv", "Frieren", None, None, 12, None, None),
    ("[Erai-raws] Jujutsu Kaisen - 24 [1080p][Multiple Subtitle].mkv", "Jujutsu Kaisen", None, None, 24, None, None),
    ("Seinfeld/Season 05/Seinfeld - S05E02-E03 - The Puffy Shirt.mkv", "Seinfeld", None, 5, 2, 3, None),
    ("Lost/Season 1/Lost.S01E01E02.Pilot.mkv", "Lost", None, 1, 1, 2, None),
    ("Movies\\Alien (1979)\\Alien (1979) - 4K.mkv", "Alien", 1979, None, None, None, None),
    ("movies/Inception.2010.1080p.BluRay.x264.YIFY.mp4", "Inception", 2010, None, None, None, None),
    ("Movies/Heat (1995)/Heat (1995) Disc 1.mkv", "Heat", 1995, None, None, None, None),
    ("The Godfather Part II (1974).avi", "The Godfather Part II", 1974, None, None, None, None),
    ("Ocean's Eleven (2001).m4v", "Ocean's Eleven", 2001, None, None, None, None),
    ("Amélie (2001)/Amélie (2001).mkv", "Amélie", 2001, None, None, None, None),
]


def norm(s):
    return "".join("and" if c == "&" else c.lower() for c in s if c == "&" or c.isalnum())


def entry(kind, i, title, year, pop):
    base = {"adult": False, "backdrop_path": f"/back{i}.jpg", "genre_ids": [18, 53], "id": i, "original_language": "en", "overview": "Recorded overview text.", "popularity": pop, "poster_path": f"/poster{i}.jpg", "vote_average": 7.4, "vote_count": 3120}
    if kind == "movie":
        return {**base, "original_title": title, "release_date": f"{year}-06-{rng.randrange(10, 28)}", "title": title, "video": False}
    return {**base, "first_air_date": f"{year}-09-{rng.randrange(10, 28)}", "name": title, "origin_country": ["US"], "original_name": title}


def response(results):
    return {"page": 1, "results": results, "total_pages": 1, "total_results": len(results)}


def library(rows):
    """200 distinct titles (140 films, 60 shows) from the corpus, and the TMDB answers for them."""
    seen, picked = set(), []
    for r in rows:
        kind = "tv" if r[4] is not None else "movie"
        want = 60 if kind == "tv" else 140
        if r[1] in seen or sum(1 for p in picked if p[0] == kind) >= want:
            continue
        seen.add(r[1])
        picked.append((kind, r))
    lib, tmdb = [], {}
    for n, (kind, r) in enumerate(picked):
        path, title, year = r[0], r[1], r[2]
        tmdb_id = 100000 + n
        key = f"{kind}|{norm(title)}|"
        true_year = year or rng.randrange(1960, 2023)
        true = entry(kind, tmdb_id, title, true_year, 40.0)
        decoy = entry(kind, tmdb_id + 500000, title, true_year - rng.randrange(8, 40), 95.0)
        other = entry(kind, tmdb_id + 900000, title + " II", true_year + 2, 60.0)
        pops = [true, other] if n % 10 else [decoy, true, other]
        tmdb[key] = response(sorted(pops, key=lambda e: -e["popularity"]))
        tmdb[key + str(true_year)] = response([true])
        lib.append((path, tmdb_id, kind))
    return lib, tmdb


def main(out):
    rows = movie_rows(1260) + show_rows(740 - len(HAND)) + HAND
    assert len(rows) == 2000, len(rows)
    rng.shuffle(rows)
    with open(f"{out}/corpus.tsv", "w", encoding="utf-8") as f:
        for r in rows:
            f.write("\t".join("" if c is None else str(c) for c in r) + "\n")
    lib, tmdb = library(rows)
    assert len(lib) == 200, len(lib)
    with open(f"{out}/library.tsv", "w", encoding="utf-8") as f:
        for p, i, kind in lib:
            f.write(f"{p}\t{i}\t{kind}\n")
    with open(f"{out}/tmdb.json", "w", encoding="utf-8") as f:
        json.dump(tmdb, f, ensure_ascii=False, sort_keys=True)


main(sys.argv[1])
